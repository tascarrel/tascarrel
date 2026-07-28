//! Supervised code-server sessions and their host-routed lifecycle.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Empty;
use hyper::Method;
use hyper::Request;
use hyper::StatusCode;
use hyper::client::conn::http1 as client_http1;
use hyper_util::rt::TokioIo;
use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_api::types::code as api;
use tascarrel_api::types::network as network_api;
use tascarrel_api::types::processes as process_api;
use tascarrel_api::types::protocol as wire;
use tascarrel_api::types::store as store_api;
use tascarrel_protocol::PodId as RuntimePodId;
use tascarrel_store::Store;
use tascarrel_store::StoreEvent;
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio::time::MissedTickBehavior;
use tokio::time::interval;
use tokio::time::timeout;
use tracing::debug;
use tracing::warn;
use uuid::Uuid;

use crate::CODE_EDITOR_PROFILE_PATH;
use crate::GuestNetworkService;
use crate::PodService;
use crate::PodServiceError;
use crate::ProcessSupervisor;
use crate::ProcessSupervisorError;
use crate::control_plane::HostClient;
use crate::control_plane::HostClientError;

const DEFAULT_MAX_SESSIONS: usize = 256;
const DEFAULT_MAX_EXTENSIONS: usize = 128;
const DEFAULT_EXTENSION_INSTALL_TIMEOUT: Duration = Duration::from_mins(5);
const DEFAULT_READINESS_TIMEOUT: Duration = Duration::from_mins(2);
const DEFAULT_PROCESS_STOP_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_PROBE_INTERVAL: Duration = Duration::from_millis(100);
const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const DEFAULT_FOLDER: &str = "/workspace";
const CODE_TITLE_MAX_BYTES: usize = 256;
const CODE_FOLDER_MAX_BYTES: usize = 4096;
const CODE_EXTENSION_ID_MAX_BYTES: usize = 256;
const CODE_SERVER_LISTEN_PREFIX: &str = "HTTP server listening on http://127.0.0.1:";
const CODE_SERVER_SEED_CONFIG_PATH: &str = "~/.tascarrel/editors/code/config";
const REQUIRED_CODE_SERVER_EXTENSIONS: &[&str] = &["github.github-vscode-theme"];
const DEFAULT_STORE_HISTORY_LIMIT: NonZeroUsize =
    NonZeroUsize::new(256).expect("the Code store history limit is non-zero");
const CODE_SERVER_SETTINGS: &str = include_str!("settings.json");
const CODE_SERVER_PREPARER: &str = r#"
umask 000
code_server=/opt/tascarrel/tools/code-server/bin/code-server
node=/opt/tascarrel/tools/code-server/libexec/code-server/lib/node
seed_config="$HOME/${TASCARREL_CODE_SERVER_SEED_CONFIG#\~/}"
profile="$HOME/${TASCARREL_CODE_SERVER_PROFILE#\~/}"
extensions="$profile/extensions"
provisioner="$profile/provisioner"
"$node" -e '
const fs = require("fs");
const [seedConfig, sharedUser, extensions, provisioner] = process.argv.slice(1);
for (const path of [sharedUser, extensions, provisioner]) {
    fs.mkdirSync(path, { recursive: true });
}
for (const filename of ["settings.json", "keyboardLayout.json"]) {
    const source = `${seedConfig}/${filename}`;
    const destination = `${sharedUser}/${filename}`;
    if (fs.existsSync(destination)) continue;
    try {
        if (fs.statSync(source).isFile()) {
            fs.copyFileSync(source, destination, fs.constants.COPYFILE_EXCL);
        }
    } catch (error) {
        if (error.code !== "ENOENT" && error.code !== "EEXIST") throw error;
    }
}
' "$seed_config" "$profile/User" "$extensions" "$provisioner"
if [ ! -e "$profile/User/settings.json" ]; then
    printf '%s\n' "$TASCARREL_CODE_SERVER_SETTINGS" > "$profile/User/settings.json"
fi
installed_extensions="$("$code_server" --user-data-dir "$provisioner" --extensions-dir "$extensions" --list-extensions)"
printf '%s\n' "$TASCARREL_CODE_SERVER_EXTENSIONS" | while IFS= read -r extension; do
    [ -n "$extension" ] || continue
    extension_installed=false
    for installed_extension in $installed_extensions; do
        if [ "$installed_extension" = "$extension" ]; then
            extension_installed=true
            break
        fi
    done
    if [ "$extension_installed" = false ]; then
        "$code_server" \
            --user-data-dir "$provisioner" \
            --extensions-dir "$extensions" \
            --install-extension "$extension"
    fi
done
"#;
const CODE_SERVER_LAUNCHER: &str = r#"
umask 000
node=/opt/tascarrel/tools/code-server/libexec/code-server/lib/node
profile="$HOME/${TASCARREL_CODE_SERVER_PROFILE#\~/}"
session_data="$HOME/.cache/tascarrel/code-server/$1"
"$node" -e '
const fs = require("fs");
const [data, sharedUser] = process.argv.slice(1);
fs.mkdirSync(data, { recursive: true });
const user = `${data}/User`;
try {
    if (fs.readlinkSync(user) === sharedUser) process.exit(0);
    fs.unlinkSync(user);
} catch (error) {
    if (error.code === "EINVAL") fs.rmSync(user, { recursive: true, force: true });
    else if (error.code !== "ENOENT") throw error;
}
fs.symlinkSync(sharedUser, user, "dir");
' "$session_data" "$profile/User"
exec /opt/tascarrel/tools/code-server/bin/code-server \
    --bind-addr "127.0.0.1:0" \
    --auth none \
    --disable-workspace-trust \
    --disable-telemetry \
    --disable-update-check \
    --ignore-last-opened \
    --user-data-dir "$session_data" \
    --extensions-dir "$profile/extensions" \
    "$2"
"#;

/// Guest-owned aggregate of supervised code-server processes and host routes.
#[derive(Clone)]
pub struct CodeService {
    inner: Arc<CodeServiceInner>,
}

impl std::fmt::Debug for CodeService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CodeService")
            .field("max_sessions", &self.inner.config.max_sessions)
            .finish_non_exhaustive()
    }
}

impl CodeService {
    /// Creates an empty workspace Code session service.
    ///
    /// # Errors
    ///
    /// Returns an error when configured limits, extension identifiers, or
    /// readiness policy are invalid.
    pub fn new(config: CodeServiceConfig) -> Result<Self, Report<CodeServiceError>> {
        config.validate()?;
        let extensions = normalize_extension_ids(&config.extensions, config.max_extensions)?;
        let store = Store::new(
            api::CodeSessionList {
                code_sessions: Vec::new().into(),
            },
            reduce_code_sessions,
            config.store_history_limit,
        );
        Ok(Self {
            inner: Arc::new(CodeServiceInner {
                config,
                extensions,
                state: Mutex::new(CodeState {
                    sessions: HashMap::new(),
                    targets: HashMap::new(),
                    store,
                }),
                provision: AsyncMutex::new(()),
                shutting_down: AtomicBool::new(false),
            }),
        })
    }

    /// Ensures one active code-server session for a workspace pod folder.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid input, exhausted capacity, service
    /// shutdown, process failure, or a rejected host route.
    #[tracing::instrument(
        level = "debug",
        skip(self, input, caller, pods, processes, network_service, host, request_context),
        fields(workspace = %input.workspace.as_str(), pod_id = %input.pod_id.0)
    )]
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "session admission coordinates explicit operation-time services and one lifecycle"
    )]
    pub(crate) async fn ensure_session(
        &self,
        input: api::EnsureCodeSessionAction,
        caller: wire::Actor,
        pods: &PodService,
        processes: &ProcessSupervisor,
        network_service: Arc<GuestNetworkService>,
        host: &HostClient,
        request_context: wire::RequestContext,
    ) -> Result<api::EnsureCodeSessionOutput, Report<CodeServiceError>> {
        self.require_running()?;
        let folder = validate_folder(input.folder.as_deref().unwrap_or(DEFAULT_FOLDER))?;
        validate_title(&input.title)?;
        let target = CodeTarget {
            workspace: input.workspace.clone(),
            pod_id: input.pod_id.clone(),
            folder: folder.clone(),
        };
        let _provision = self.inner.provision.lock().await;
        self.require_running()?;

        let existing = {
            let state = lock(&self.inner.state);
            state
                .targets
                .get(&target)
                .and_then(|id| state.sessions.get(id))
                .map(|entry| entry.session.clone())
        };
        if let Some(session) = existing.as_ref()
            && matches!(
                session.status,
                api::CodeSessionStatus::Starting | api::CodeSessionStatus::Running
            )
        {
            let route = create_route(host, request_context, session, &input.title).await?;
            if route.http_route_id != session.http_route_id
                || route.hostname_prefix != session.hostname_prefix
                || session.title != input.title
            {
                self.update_route_metadata(&session.id, &session.process_id, input.title, route);
            }
            return Ok(api::EnsureCodeSessionOutput {
                code_session_id: session.id.clone(),
            });
        }

        let session_id = if let Some(session) = existing.as_ref() {
            processes
                .terminate_and_remove(&session.process_id, self.inner.config.process_stop_timeout)
                .await
                .map_err(process_error)?;
            delete_route_if_present(host, request_context.clone(), &session.http_route_id).await?;
            session.id.clone()
        } else {
            let state = lock(&self.inner.state);
            if state.sessions.len() >= self.inner.config.max_sessions {
                return Err(overloaded("Code session capacity has been reached"));
            }
            unique_code_session_id(&state)
        };

        self.prepare_profile(
            &input,
            caller.clone(),
            pods,
            processes,
            Arc::clone(&network_service),
        )
        .await?;
        let spawned = processes
            .spawn(
                spawn_action(&input, &folder, &session_id),
                caller,
                pods,
                network_service,
            )
            .map_err(process_error)?;
        let pod_port = match timeout(
            self.inner.config.readiness_timeout,
            discover_code_server_port(processes, &spawned.process_id),
        )
        .await
        {
            Ok(Ok(port)) => port,
            Ok(Err(error)) => {
                stop_failed_start(processes, &spawned.process_id, &self.inner.config).await;
                return Err(error);
            }
            Err(_) => {
                stop_failed_start(processes, &spawned.process_id, &self.inner.config).await;
                return Err(unavailable(
                    "timed out waiting for code-server to report its pod port",
                ));
            }
        };
        let route = match create_route_for_target(
            host,
            request_context,
            &input.workspace,
            &input.pod_id,
            pod_port,
            input.title.clone(),
        )
        .await
        {
            Ok(route) => route,
            Err(error) => {
                stop_failed_start(processes, &spawned.process_id, &self.inner.config).await;
                return Err(error);
            }
        };
        let session = api::CodeSession {
            id: session_id.clone(),
            workspace: input.workspace,
            pod_id: input.pod_id,
            folder: folder.into(),
            title: input.title,
            pod_port,
            process_id: spawned.process_id.clone(),
            http_route_id: route.http_route_id,
            hostname_prefix: route.hostname_prefix,
            status: api::CodeSessionStatus::Starting,
        };
        {
            let mut state = lock(&self.inner.state);
            if let Some(entry) = state.sessions.remove(&session_id)
                && let Some(task) = entry.monitor
            {
                task.abort();
            }
            state.targets.insert(target, session_id.clone());
            state.sessions.insert(
                session_id.clone(),
                CodeSessionEntry {
                    session: session.clone(),
                    monitor: None,
                },
            );
            state
                .store
                .apply(api::CodeSessionListMutation::Upsert(session.clone()));
        }
        self.restart_monitor(
            &session_id,
            &spawned.process_id,
            session.pod_id,
            pod_port,
            pods.clone(),
            processes.clone(),
        );
        Ok(api::EnsureCodeSessionOutput {
            code_session_id: session_id,
        })
    }

    /// Deletes one Code session and releases its process and host route.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown identifier or service shutdown.
    pub(crate) async fn delete_session(
        &self,
        input: &api::DeleteCodeSessionAction,
        processes: &ProcessSupervisor,
        host: &HostClient,
        request_context: wire::RequestContext,
    ) -> Result<api::DeleteCodeSessionOutput, Report<CodeServiceError>> {
        self.require_running()?;
        let entry = {
            let mut state = lock(&self.inner.state);
            let entry = state
                .sessions
                .remove(&input.code_session_id)
                .ok_or_else(|| {
                    invalid_request(format!(
                        "Code session {} does not exist",
                        input.code_session_id.0
                    ))
                })?;
            state.targets.remove(&CodeTarget::from(&entry.session));
            state.store.apply(api::CodeSessionListMutation::Remove(
                entry.session.id.clone(),
            ));
            entry
        };
        if let Some(task) = entry.monitor {
            task.abort();
        }
        if let Err(error) = processes
            .terminate_and_remove(
                &entry.session.process_id,
                self.inner.config.process_stop_timeout,
            )
            .await
        {
            warn!(%error, code_session_id = %entry.session.id.0, "could not stop deleted Code session process");
        }
        if let Err(error) = host
            .execute(
                request_context,
                network_api::DeleteHttpRouteAction {
                    http_route_id: entry.session.http_route_id,
                },
            )
            .await
        {
            warn!(%error, code_session_id = %entry.session.id.0, "could not delete Code session route");
        }
        Ok(api::DeleteCodeSessionOutput {})
    }

    /// Opens a resumable subscription to this workspace's Code sessions.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid cursor.
    pub(crate) fn subscribe_sessions(
        &self,
        input: &api::CodeSessionListChangedSubscription,
    ) -> Result<CodeSessionListSubscription, Report<CodeServiceError>> {
        let cursor = input.cursor.as_ref().map(runtime_stamp).transpose()?;
        Ok(lock(&self.inner.state).store.subscribe(cursor))
    }

    /// Stops background monitors and rejects future mutations.
    pub async fn shutdown(&self) {
        if self.inner.shutting_down.swap(true, Ordering::AcqRel) {
            return;
        }
        let tasks = {
            let mut state = lock(&self.inner.state);
            state
                .sessions
                .values_mut()
                .filter_map(|entry| entry.monitor.take())
                .collect::<Vec<_>>()
        };
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            if let Err(error) = task.await
                && !error.is_cancelled()
            {
                warn!(%error, "Code session monitor failed during shutdown");
            }
        }
    }

    async fn prepare_profile(
        &self,
        input: &api::EnsureCodeSessionAction,
        caller: wire::Actor,
        pods: &PodService,
        processes: &ProcessSupervisor,
        network_service: Arc<GuestNetworkService>,
    ) -> Result<(), Report<CodeServiceError>> {
        let process = processes
            .spawn(
                prepare_action(input, &self.inner.extensions),
                caller,
                pods,
                network_service,
            )
            .map_err(process_error)?;
        match timeout(
            self.inner.config.extension_install_timeout,
            processes.wait_for_success(&process.process_id),
        )
        .await
        {
            Ok(Ok(())) => {
                processes
                    .remove(process_api::RemoveProcessAction {
                        process_id: process.process_id,
                    })
                    .map_err(process_error)?;
                Ok(())
            }
            Ok(Err(error)) => {
                Err(process_error(error)
                    .message("failed to prepare the shared Code editor profile"))
            }
            Err(_) => {
                if let Err(error) = processes
                    .terminate_and_remove(
                        &process.process_id,
                        self.inner.config.process_stop_timeout,
                    )
                    .await
                {
                    warn!(%error, "could not stop timed-out Code extension installer");
                }
                Err(unavailable("timed out installing Code editor extensions"))
            }
        }
    }

    fn require_running(&self) -> Result<(), Report<CodeServiceError>> {
        if self.inner.shutting_down.load(Ordering::Acquire) {
            Err(unavailable("Code service is shutting down"))
        } else {
            Ok(())
        }
    }

    fn update_route_metadata(
        &self,
        session_id: &api::CodeSessionId,
        process_id: &process_api::ProcessId,
        title: tascarrel_api::ArcStr,
        route: network_api::CreateHttpRouteOutput,
    ) {
        let mut state = lock(&self.inner.state);
        let Some(entry) = state.sessions.get_mut(session_id) else {
            return;
        };
        if entry.session.process_id != *process_id {
            return;
        }
        entry.session.title = title;
        entry.session.http_route_id = route.http_route_id;
        entry.session.hostname_prefix = route.hostname_prefix;
        let session = entry.session.clone();
        state
            .store
            .apply(api::CodeSessionListMutation::Upsert(session));
    }

    #[allow(clippy::too_many_arguments)]
    fn restart_monitor(
        &self,
        session_id: &api::CodeSessionId,
        process_id: &process_api::ProcessId,
        pod_id: tascarrel_api::types::pods::PodId,
        pod_port: u16,
        pods: PodService,
        processes: ProcessSupervisor,
    ) {
        let inner = Arc::clone(&self.inner);
        let monitor_session_id = session_id.clone();
        let monitor_process_id = process_id.clone();
        let task = tokio::spawn(async move {
            monitor_session(
                inner,
                monitor_session_id,
                monitor_process_id,
                pod_id,
                pod_port,
                pods,
                processes,
            )
            .await;
        });
        let mut state = lock(&self.inner.state);
        if let Some(entry) = state.sessions.get_mut(session_id)
            && entry.session.process_id == *process_id
        {
            if let Some(previous) = entry.monitor.replace(task) {
                previous.abort();
            }
        } else {
            task.abort();
        }
    }
}

/// Limits, extensions, and readiness policy for workspace Code sessions.
#[derive(Clone, Debug)]
pub struct CodeServiceConfig {
    /// Maximum number of retained Code sessions.
    pub max_sessions: usize,
    /// Maximum number of required and workspace-configured extensions.
    pub max_extensions: usize,
    /// Additional workspace-wide Marketplace extension identifiers.
    pub extensions: Vec<String>,
    /// Maximum time allowed to install missing extensions.
    pub extension_install_timeout: Duration,
    /// Maximum time allowed for code-server to announce its port or pass its
    /// health probe.
    pub readiness_timeout: Duration,
    /// Maximum graceful and forced waits while releasing one editor process.
    pub process_stop_timeout: Duration,
    /// Delay between code-server health probes.
    pub probe_interval: Duration,
    /// Maximum duration of one code-server health probe.
    pub probe_timeout: Duration,
    /// Number of mutations retained by the Code session store.
    pub store_history_limit: NonZeroUsize,
}

impl Default for CodeServiceConfig {
    fn default() -> Self {
        Self {
            max_sessions: DEFAULT_MAX_SESSIONS,
            max_extensions: DEFAULT_MAX_EXTENSIONS,
            extensions: Vec::new(),
            extension_install_timeout: DEFAULT_EXTENSION_INSTALL_TIMEOUT,
            readiness_timeout: DEFAULT_READINESS_TIMEOUT,
            process_stop_timeout: DEFAULT_PROCESS_STOP_TIMEOUT,
            probe_interval: DEFAULT_PROBE_INTERVAL,
            probe_timeout: DEFAULT_PROBE_TIMEOUT,
            store_history_limit: DEFAULT_STORE_HISTORY_LIMIT,
        }
    }
}

impl CodeServiceConfig {
    fn validate(&self) -> Result<(), Report<CodeServiceError>> {
        if self.max_sessions == 0
            || self.max_extensions < REQUIRED_CODE_SERVER_EXTENSIONS.len()
            || self.extension_install_timeout.is_zero()
            || self.readiness_timeout.is_zero()
            || self.process_stop_timeout.is_zero()
            || self.probe_interval.is_zero()
            || self.probe_timeout.is_zero()
        {
            return Err(CodeServiceError::InvalidConfiguration.report());
        }
        Ok(())
    }
}

/// Caller-relevant Code service failure categories.
#[derive(Debug, Error)]
pub enum CodeServiceError {
    /// The service was constructed with invalid limits or editor configuration.
    #[error("Code service configuration is invalid")]
    InvalidConfiguration,
    /// An action input, cursor, or session identifier is invalid.
    #[error("invalid Code request: {0}")]
    InvalidRequest(String),
    /// A pod or required daemon service is unavailable.
    #[error("Code session dependency is unavailable: {0}")]
    Unavailable(String),
    /// A configured Code or network resource limit has been reached.
    #[error("Code service is overloaded: {0}")]
    Overloaded(String),
    /// Aggregate state or a nested service operation failed unexpectedly.
    #[error("Code service failed: {0}")]
    Internal(String),
}

/// Resumable stream of Code session-list changes for one workspace.
pub(crate) type CodeSessionListSubscription =
    tascarrel_store::Subscription<api::CodeSessionList, api::CodeSessionListMutation>;

type CodeSessionStore = Store<api::CodeSessionList, api::CodeSessionListMutation>;

struct CodeServiceInner {
    config: CodeServiceConfig,
    extensions: Vec<String>,
    state: Mutex<CodeState>,
    provision: AsyncMutex<()>,
    shutting_down: AtomicBool,
}

struct CodeState {
    sessions: HashMap<api::CodeSessionId, CodeSessionEntry>,
    targets: HashMap<CodeTarget, api::CodeSessionId>,
    store: CodeSessionStore,
}

struct CodeSessionEntry {
    session: api::CodeSession,
    monitor: Option<JoinHandle<()>>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CodeTarget {
    workspace: tascarrel_api::types::workspaces::WorkspaceName,
    pod_id: tascarrel_api::types::pods::PodId,
    folder: String,
}

impl From<&api::CodeSession> for CodeTarget {
    fn from(session: &api::CodeSession) -> Self {
        Self {
            workspace: session.workspace.clone(),
            pod_id: session.pod_id.clone(),
            folder: session.folder.to_string(),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn monitor_session(
    inner: Arc<CodeServiceInner>,
    session_id: api::CodeSessionId,
    process_id: process_api::ProcessId,
    pod_id: tascarrel_api::types::pods::PodId,
    pod_port: u16,
    pods: PodService,
    processes: ProcessSupervisor,
) {
    let mut process_events = match processes
        .subscribe_process_list(process_api::ProcessListChangedSubscription { cursor: None })
    {
        Ok(events) => events,
        Err(error) => {
            update_status(
                &inner,
                &session_id,
                &process_id,
                failed_status(format!("could not monitor code-server: {error}")),
            );
            return;
        }
    };
    let deadline = Instant::now() + inner.config.readiness_timeout;
    let mut probes = interval(inner.config.probe_interval);
    probes.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut ready = false;
    let mut last_probe_error = None;

    loop {
        tokio::select! {
            event = process_events.recv() => {
                let Some(event) = event else {
                    update_status(
                        &inner,
                        &session_id,
                        &process_id,
                        failed_status("code-server process monitoring ended"),
                    );
                    return;
                };
                if let Some(status) = observed_process_status(&event, &process_id) {
                    match status {
                        ObservedProcessStatus::Active => {}
                        ObservedProcessStatus::Exited => {
                            update_status(
                                &inner,
                                &session_id,
                                &process_id,
                                api::CodeSessionStatus::Exited,
                            );
                            return;
                        }
                        ObservedProcessStatus::Failed(message) => {
                            update_status(
                                &inner,
                                &session_id,
                                &process_id,
                                failed_status(message),
                            );
                            return;
                        }
                        ObservedProcessStatus::Missing => {
                            update_status(
                                &inner,
                                &session_id,
                                &process_id,
                                failed_status("code-server disappeared from process supervision"),
                            );
                            return;
                        }
                    }
                }
            }
            _ = probes.tick(), if !ready => {
                match probe_code_server(&pods, &pod_id, pod_port, inner.config.probe_timeout).await {
                    Ok(true) => {
                        ready = true;
                        update_status(
                            &inner,
                            &session_id,
                            &process_id,
                            api::CodeSessionStatus::Running,
                        );
                    }
                    Ok(false) => last_probe_error = Some("code-server health endpoint is not ready".to_owned()),
                    Err(error) => last_probe_error = Some(error.to_string()),
                }
                if !ready && Instant::now() >= deadline {
                    let detail = last_probe_error
                        .unwrap_or_else(|| "code-server did not become ready".to_owned());
                    if let Err(error) = processes.kill(process_api::KillProcessAction {
                        process_id: process_id.clone(),
                        signal: process_api::ProcessSignal::Terminate,
                    }) {
                        warn!(%error, code_session_id = %session_id.0, "could not stop code-server after readiness timed out");
                    }
                    update_status(
                        &inner,
                        &session_id,
                        &process_id,
                        failed_status(format!("code-server readiness timed out: {detail}")),
                    );
                    return;
                }
            }
        }
    }
}

async fn probe_code_server(
    pods: &PodService,
    pod_id: &tascarrel_api::types::pods::PodId,
    pod_port: u16,
    probe_timeout: Duration,
) -> Result<bool, Report<CodeServiceError>> {
    let probe = async {
        let runtime_pod = RuntimePodId(pod_id.0.to_string());
        let channel = pods
            .connect_port(&runtime_pod, pod_port)
            .await
            .map_err(pod_error)?;
        let (mut sender, connection) = client_http1::handshake(TokioIo::new(channel))
            .await
            .map_err(|error| unavailable(format!("failed to open code-server probe: {error}")))?;
        let connection = tokio::spawn(async move {
            if let Err(error) = connection.await {
                debug!(%error, "code-server probe connection stopped");
            }
        });
        let request = Request::builder()
            .method(Method::GET)
            .uri("/healthz")
            .header(hyper::header::HOST, "localhost")
            .body(Empty::<Bytes>::new())
            .map_err(|error| internal(format!("failed to build code-server probe: {error}")))?;
        let response = sender
            .send_request(request)
            .await
            .map_err(|error| unavailable(format!("failed to send code-server probe: {error}")));
        connection.abort();
        Ok::<_, Report<CodeServiceError>>(response?.status() == StatusCode::OK)
    };
    timeout(probe_timeout, probe)
        .await
        .map_err(|_| unavailable("timed out probing code-server"))?
}

enum ObservedProcessStatus {
    Active,
    Exited,
    Failed(String),
    Missing,
}

fn observed_process_status(
    event: &StoreEvent<process_api::ProcessList, process_api::ProcessListMutation>,
    process_id: &process_api::ProcessId,
) -> Option<ObservedProcessStatus> {
    match event {
        StoreEvent::Snapshot(snapshot) => Some(
            snapshot
                .value
                .processes
                .iter()
                .find(|process| process.id == *process_id)
                .map_or(ObservedProcessStatus::Missing, |process| {
                    process_status(&process.status)
                }),
        ),
        StoreEvent::Mutation(mutation) => match mutation.mutation.as_ref() {
            process_api::ProcessListMutation::Upsert(process) if process.id == *process_id => {
                Some(process_status(&process.status))
            }
            process_api::ProcessListMutation::Remove(id) if id == process_id => {
                Some(ObservedProcessStatus::Missing)
            }
            _ => None,
        },
    }
}

fn process_status(status: &process_api::ProcessState) -> ObservedProcessStatus {
    match status {
        process_api::ProcessState::Starting
        | process_api::ProcessState::Running
        | process_api::ProcessState::Stopping => ObservedProcessStatus::Active,
        process_api::ProcessState::Exited(_) => ObservedProcessStatus::Exited,
        process_api::ProcessState::Failed(failure) => {
            ObservedProcessStatus::Failed(failure.message.to_string())
        }
    }
}

fn update_status(
    inner: &CodeServiceInner,
    session_id: &api::CodeSessionId,
    process_id: &process_api::ProcessId,
    status: api::CodeSessionStatus,
) {
    let mut state = lock(&inner.state);
    let Some(entry) = state.sessions.get_mut(session_id) else {
        return;
    };
    if entry.session.process_id != *process_id || entry.session.status == status {
        return;
    }
    entry.session.status = status;
    let session = entry.session.clone();
    state
        .store
        .apply(api::CodeSessionListMutation::Upsert(session));
}

async fn discover_code_server_port(
    processes: &ProcessSupervisor,
    process_id: &process_api::ProcessId,
) -> Result<u16, Report<CodeServiceError>> {
    let mut logs = processes
        .subscribe_log(process_api::ProcessLogSubscription {
            process_id: process_id.clone(),
            last_line: None,
        })
        .map_err(|error| {
            error.escalate(CodeServiceError::Internal(
                "could not observe code-server startup".to_owned(),
            ))
        })?;
    while let Some(event) = logs.recv().await {
        for line in &event.lines {
            if !line.truncated
                && let Some(port) = parse_code_server_port(&line.content)
            {
                return Ok(port);
            }
        }
    }
    Err(unavailable(
        "code-server stopped before reporting its pod port",
    ))
}

fn parse_code_server_port(line: &str) -> Option<u16> {
    let (_, address) = line.split_once(CODE_SERVER_LISTEN_PREFIX)?;
    let port = address.trim().strip_suffix('/')?.parse::<u16>().ok()?;
    (port != 0).then_some(port)
}

async fn stop_failed_start(
    processes: &ProcessSupervisor,
    process_id: &process_api::ProcessId,
    config: &CodeServiceConfig,
) {
    if let Err(error) = processes
        .terminate_and_remove(process_id, config.process_stop_timeout)
        .await
    {
        warn!(%error, process_id = %process_id.0, "could not stop code-server after startup failed");
    }
}

async fn delete_route_if_present(
    host: &HostClient,
    request_context: wire::RequestContext,
    http_route_id: &network_api::HttpRouteId,
) -> Result<(), Report<CodeServiceError>> {
    match host
        .execute(
            request_context,
            network_api::DeleteHttpRouteAction {
                http_route_id: http_route_id.clone(),
            },
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(error)
            if matches!(
                error.error(),
                HostClientError::Remote(wire::OperationError::InvalidRequest(_))
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(host_error(error).message("failed to release the previous Code route")),
    }
}

async fn create_route(
    host: &HostClient,
    request_context: wire::RequestContext,
    session: &api::CodeSession,
    title: &str,
) -> Result<network_api::CreateHttpRouteOutput, Report<CodeServiceError>> {
    create_route_for_target(
        host,
        request_context,
        &session.workspace,
        &session.pod_id,
        session.pod_port,
        title.into(),
    )
    .await
}

async fn create_route_for_target(
    host: &HostClient,
    request_context: wire::RequestContext,
    workspace: &tascarrel_api::types::workspaces::WorkspaceName,
    pod_id: &tascarrel_api::types::pods::PodId,
    pod_port: u16,
    title: tascarrel_api::ArcStr,
) -> Result<network_api::CreateHttpRouteOutput, Report<CodeServiceError>> {
    host.execute(
        request_context,
        network_api::CreateHttpRouteAction {
            workspace: workspace.clone(),
            pod_id: pod_id.clone(),
            pod_port,
            title,
            internal: true,
        },
    )
    .await
    .map_err(host_error)
}

fn normalize_extension_ids(
    configured: &[String],
    max_extensions: usize,
) -> Result<Vec<String>, Report<CodeServiceError>> {
    let mut seen = BTreeSet::new();
    let mut extensions = Vec::new();
    for extension in REQUIRED_CODE_SERVER_EXTENSIONS
        .iter()
        .copied()
        .chain(configured.iter().map(String::as_str))
    {
        let extension = normalize_extension_id(extension)?;
        if !seen.insert(extension.clone()) {
            continue;
        }
        if extensions.len() >= max_extensions {
            return Err(invalid_request(format!(
                "Code editor config may select at most {max_extensions} unique extensions"
            )));
        }
        extensions.push(extension);
    }
    Ok(extensions)
}

fn normalize_extension_id(extension: &str) -> Result<String, Report<CodeServiceError>> {
    let valid = extension.len() <= CODE_EXTENSION_ID_MAX_BYTES
        && extension.split_once('.').is_some_and(|(publisher, name)| {
            !name.contains('.')
                && valid_extension_component(publisher)
                && valid_extension_component(name)
        });
    if !valid {
        return Err(invalid_request(
            "Code extensions must be Marketplace identifiers in publisher.name form",
        ));
    }
    Ok(extension.to_ascii_lowercase())
}

fn valid_extension_component(component: &str) -> bool {
    !component.is_empty()
        && component
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        && component
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && component
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

fn prepare_action(
    input: &api::EnsureCodeSessionAction,
    extensions: &[String],
) -> process_api::SpawnProcessAction {
    let mut environment = HashMap::new();
    environment.insert(
        "TASCARREL_CODE_SERVER_SETTINGS".into(),
        CODE_SERVER_SETTINGS.into(),
    );
    environment.insert(
        "TASCARREL_CODE_SERVER_EXTENSIONS".into(),
        extensions.join("\n").into(),
    );
    environment.insert(
        "TASCARREL_CODE_SERVER_SEED_CONFIG".into(),
        CODE_SERVER_SEED_CONFIG_PATH.into(),
    );
    environment.insert(
        "TASCARREL_CODE_SERVER_PROFILE".into(),
        CODE_EDITOR_PROFILE_PATH.into(),
    );
    process_api::SpawnProcessAction {
        pod_id: input.pod_id.clone(),
        start_pod: Some(true),
        title: "Prepare Code editor".into(),
        executable: "/bin/sh".into(),
        arguments: vec!["-eu".into(), "-c".into(), CODE_SERVER_PREPARER.into()].into(),
        environment,
        working_directory: Some("/workspace".into()),
        terminal: None,
        log_stdout: Some(true),
        profile: process_api::ProcessExecutionProfile::User,
    }
}

fn spawn_action(
    input: &api::EnsureCodeSessionAction,
    folder: &str,
    session_id: &api::CodeSessionId,
) -> process_api::SpawnProcessAction {
    let mut environment = HashMap::new();
    environment.insert(
        "TASCARREL_CODE_SERVER_PROFILE".into(),
        CODE_EDITOR_PROFILE_PATH.into(),
    );
    process_api::SpawnProcessAction {
        pod_id: input.pod_id.clone(),
        start_pod: Some(true),
        title: input.title.clone(),
        executable: "/bin/sh".into(),
        arguments: vec![
            "-eu".into(),
            "-c".into(),
            CODE_SERVER_LAUNCHER.into(),
            "tascarrel-code-server".into(),
            session_id.0.to_string().into(),
            folder.into(),
        ]
        .into(),
        environment,
        working_directory: Some(folder.into()),
        terminal: None,
        log_stdout: Some(true),
        profile: process_api::ProcessExecutionProfile::User,
    }
}

fn unique_code_session_id(state: &CodeState) -> api::CodeSessionId {
    loop {
        let id = api::CodeSessionId::generate();
        if !state.sessions.contains_key(&id) {
            return id;
        }
    }
}

fn validate_title(title: &str) -> Result<(), Report<CodeServiceError>> {
    if title.trim().is_empty()
        || title.len() > CODE_TITLE_MAX_BYTES
        || title.chars().any(char::is_control)
    {
        return Err(invalid_request(
            "Code title must contain 1-256 bytes without control characters",
        ));
    }
    Ok(())
}

fn validate_folder(folder: &str) -> Result<String, Report<CodeServiceError>> {
    if !folder.starts_with('/')
        || folder.len() > CODE_FOLDER_MAX_BYTES
        || folder.chars().any(char::is_control)
    {
        return Err(invalid_request(
            "Code folder must be an absolute pod path without control characters",
        ));
    }
    let mut normalized = PathBuf::from("/");
    for component in Path::new(folder).components() {
        match component {
            Component::RootDir => {}
            Component::Normal(component) => normalized.push(component),
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(invalid_request(
                    "Code folder must be an absolute normalized pod path",
                ));
            }
        }
    }
    Ok(normalized.to_string_lossy().into_owned())
}

fn runtime_stamp(
    stamp: &store_api::Stamp,
) -> Result<tascarrel_store::Stamp, Report<CodeServiceError>> {
    let generation = stamp.generation.parse::<Uuid>().map_err(|error| {
        invalid_request("Code-session cursor generation is invalid").message(error.to_string())
    })?;
    Ok(tascarrel_store::Stamp {
        generation,
        version: stamp.version,
    })
}

fn reduce_code_sessions(list: &mut api::CodeSessionList, mutation: &api::CodeSessionListMutation) {
    match mutation {
        api::CodeSessionListMutation::Upsert(session) => {
            if let Some(index) = list
                .code_sessions
                .iter()
                .position(|entry| entry.id == session.id)
            {
                list.code_sessions[index] = session.clone();
            } else {
                list.code_sessions.push(session.clone());
                list.code_sessions
                    .sort_by(|left, right| left.id.cmp(&right.id));
            }
        }
        api::CodeSessionListMutation::Remove(id) => {
            if let Some(index) = list.code_sessions.iter().position(|entry| entry.id == *id) {
                list.code_sessions.remove(index);
            }
        }
    }
}

fn failed_status(message: impl Into<String>) -> api::CodeSessionStatus {
    api::CodeSessionStatus::Failed(api::CodeSessionFailure {
        message: message.into().into(),
    })
}

fn process_error(report: Report<ProcessSupervisorError>) -> Report<CodeServiceError> {
    let error = match report.error() {
        ProcessSupervisorError::InvalidRequest(message) => {
            CodeServiceError::InvalidRequest(message.clone())
        }
        ProcessSupervisorError::Internal(message) => CodeServiceError::Internal(message.clone()),
    };
    report.escalate(error)
}

fn pod_error(report: Report<PodServiceError>) -> Report<CodeServiceError> {
    let error = match report.error() {
        PodServiceError::InvalidRequest(message) => CodeServiceError::Unavailable(message.clone()),
        PodServiceError::Internal(message) => CodeServiceError::Internal(message.clone()),
    };
    report.escalate(error)
}

fn host_error(report: Report<HostClientError>) -> Report<CodeServiceError> {
    let error = match report.error() {
        HostClientError::Remote(remote) => match remote {
            wire::OperationError::InvalidRequest(details)
            | wire::OperationError::Internal(details)
            | wire::OperationError::Forbidden(details) => {
                CodeServiceError::Internal(details.message.to_string())
            }
            wire::OperationError::Unavailable(details)
            | wire::OperationError::TimedOut(details) => {
                CodeServiceError::Unavailable(details.message.to_string())
            }
            wire::OperationError::Overloaded(details) => {
                CodeServiceError::Overloaded(details.message.to_string())
            }
        },
        HostClientError::Unavailable
        | HostClientError::ConnectionClosed
        | HostClientError::Canceled
        | HostClientError::ControlPlane => CodeServiceError::Unavailable(report.to_string()),
        HostClientError::InvalidInput
        | HostClientError::InvalidOutput
        | HostClientError::InvalidResponse => CodeServiceError::Internal(report.to_string()),
    };
    report.escalate(error)
}

fn invalid_request(message: impl Into<String>) -> Report<CodeServiceError> {
    CodeServiceError::InvalidRequest(message.into()).report()
}

fn unavailable(message: impl Into<String>) -> Report<CodeServiceError> {
    CodeServiceError::Unavailable(message.into()).report()
}

fn overloaded(message: impl Into<String>) -> Report<CodeServiceError> {
    CodeServiceError::Overloaded(message.into()).report()
}

fn internal(message: impl Into<String>) -> Report<CodeServiceError> {
    CodeServiceError::Internal(message.into()).report()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(error) => {
            tracing::error!("Code service state mutex was poisoned");
            error.into_inner()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies profile preparation imports seed configuration and installs the
    /// required theme extension into the shared workspace mount.
    #[test]
    fn profile_preparation_uses_the_workspace_share() {
        let input = input();
        let extensions = vec!["github.github-vscode-theme".to_owned()];
        let action = prepare_action(&input, &extensions);

        assert_eq!(action.profile, process_api::ProcessExecutionProfile::User);
        assert!(
            CODE_SERVER_PREPARER
                .contains("profile=\"$HOME/${TASCARREL_CODE_SERVER_PROFILE#\\~/}\"")
        );
        assert!(
            CODE_SERVER_PREPARER
                .contains("seed_config=\"$HOME/${TASCARREL_CODE_SERVER_SEED_CONFIG#\\~/}\"")
        );
        assert!(CODE_SERVER_PREPARER.contains("libexec/code-server/lib/node"));
        assert!(CODE_SERVER_PREPARER.contains("\"settings.json\", \"keyboardLayout.json\""));
        assert!(CODE_SERVER_PREPARER.contains("--install-extension \"$extension\""));
        assert_eq!(
            action.environment["TASCARREL_CODE_SERVER_SEED_CONFIG"].as_ref(),
            CODE_SERVER_SEED_CONFIG_PATH
        );
        assert_eq!(
            action.environment["TASCARREL_CODE_SERVER_PROFILE"].as_ref(),
            CODE_EDITOR_PROFILE_PATH
        );
        assert_eq!(
            action.environment["TASCARREL_CODE_SERVER_EXTENSIONS"].as_ref(),
            "github.github-vscode-theme"
        );
    }

    /// Verifies code-server is loopback-only, trusts the workspace, isolates
    /// runtime data, and links to the shared user profile prepared by guestd.
    #[test]
    fn code_server_process_uses_private_runtime_and_shared_user_data() {
        let session_id = api::CodeSessionId::generate();
        let action = spawn_action(&input(), "/workspace", &session_id);

        assert_eq!(action.executable.as_ref(), "/bin/sh");
        assert_eq!(action.profile, process_api::ProcessExecutionProfile::User);
        assert!(action.terminal.is_none());
        assert_eq!(action.working_directory.as_deref(), Some("/workspace"));
        assert_eq!(action.arguments[4].as_ref(), session_id.0.as_ref());
        let launcher = action.arguments[2].as_ref();
        assert!(launcher.contains("--bind-addr \"127.0.0.1:0\""));
        assert!(launcher.contains("--auth none"));
        assert!(launcher.contains("--disable-workspace-trust"));
        assert!(launcher.contains("profile=\"$HOME/${TASCARREL_CODE_SERVER_PROFILE#\\~/}\""));
        assert!(launcher.contains("session_data=\"$HOME/.cache/tascarrel/code-server/$1\""));
        assert!(launcher.contains("fs.symlinkSync(sharedUser, user, \"dir\")"));
        assert!(launcher.contains("--user-data-dir \"$session_data\""));
        assert_eq!(
            action.environment["TASCARREL_CODE_SERVER_PROFILE"].as_ref(),
            CODE_EDITOR_PROFILE_PATH
        );
    }

    /// Verifies only code-server's loopback startup announcement produces a
    /// usable dynamically allocated port.
    #[test]
    fn code_server_port_is_discovered_from_its_startup_log() {
        assert_eq!(
            parse_code_server_port(
                "[2026-07-21T06:22:27.244Z] info  HTTP server listening on http://127.0.0.1:33597/"
            ),
            Some(33_597)
        );
        assert_eq!(
            parse_code_server_port("HTTP server listening on http://127.0.0.1:0/"),
            None
        );
        assert_eq!(
            parse_code_server_port("HTTP server listening on http://0.0.0.0:33597/"),
            None
        );
    }

    /// Verifies required extensions remain first while configured identifiers
    /// are normalized and deduplicated.
    #[test]
    fn extension_selection_includes_required_theme_dependencies() {
        let configured = vec![
            "Rust-Lang.Rust-Analyzer".to_owned(),
            "GitHub.github-vscode-theme".to_owned(),
        ];

        let extensions = normalize_extension_ids(&configured, 3).unwrap();

        assert_eq!(
            extensions,
            ["github.github-vscode-theme", "rust-lang.rust-analyzer"]
        );
    }

    fn input() -> api::EnsureCodeSessionAction {
        api::EnsureCodeSessionAction {
            workspace: tascarrel_api::types::workspaces::WorkspaceName::new("demo"),
            pod_id: tascarrel_api::types::pods::PodId::generate(),
            title: "Code".into(),
            folder: Some("/workspace".into()),
        }
    }
}
