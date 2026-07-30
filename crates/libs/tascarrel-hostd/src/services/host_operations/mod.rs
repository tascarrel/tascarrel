//! Durable approval-gated commands executed directly by the host daemon.

mod catalog;
mod plan;
mod storage;

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::fs;
use std::future;
use std::os::unix::process::ExitStatusExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::str::FromStr as _;
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use reportify::ErrorExt as _;
use reportify::Report;
use serde::Deserialize;
use serde::Serialize;
use sha2::Digest as _;
use sha2::Sha256;
use tascarrel_api::ProcessTerminalData;
use tascarrel_api::types::host_operations as api;
use tascarrel_api::types::pods::PodId;
use tascarrel_api::types::protocol::Actor;
use tascarrel_api::types::workspaces::WorkspaceName;
use tascarrel_protocol::ErrorCode;
use tascarrel_protocol::Framed;
use tascarrel_protocol::HostOperationInputRequest;
use tascarrel_protocol::HostOperationInputResponse;
use tascarrel_protocol::MAX_HOST_OPERATION_INPUT_BYTES;
use tascarrel_protocol::RemoteError;
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::sync::broadcast;
use tokio::sync::watch;
use tokio::task::JoinSet;

pub use self::catalog::HostCommandSubscription;
use self::plan::expand_argument;
use self::plan::expand_working_directory;
use self::plan::pending_input_list;
use self::plan::resolve_environment;
use self::plan::resolve_executable;
use self::plan::resolve_execution_environment;
use self::plan::resolve_inputs;
use self::plan::resolve_parameters;
use self::plan::validate_command;
use self::storage::OperationStorage;

const OUTPUT_CHUNK_BYTES: usize = 16 * 1024;
const PROCESS_TERMINATION_GRACE: Duration = Duration::from_secs(3);

/// Configuration required by [`HostOperationService`].
#[derive(Clone, Debug)]
pub struct HostOperationServiceConfig {
    /// Private durable state directory.
    pub state_directory: PathBuf,
    /// Host Git executable used to verify and materialize repository bundles.
    pub git: PathBuf,
}

impl HostOperationServiceConfig {
    /// Creates service configuration.
    #[must_use]
    pub fn new(state_directory: impl Into<PathBuf>, git: impl Into<PathBuf>) -> Self {
        Self {
            state_directory: state_directory.into(),
            git: git.into(),
        }
    }
}

/// Durable host-operation coordinator.
#[derive(Clone)]
pub struct HostOperationService {
    inner: Arc<HostOperationServiceInner>,
}

struct HostOperationServiceInner {
    git: PathBuf,
    storage: OperationStorage,
    operations: Mutex<BTreeMap<api::HostOperationId, StoredOperation>>,
    generation: watch::Sender<u64>,
    stream_events: broadcast::Sender<StreamNotice>,
    running: Mutex<HashMap<api::HostOperationId, watch::Sender<bool>>>,
    state_transitions: Mutex<()>,
    audit_writes: Mutex<()>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct StoredOperation {
    operation: api::HostOperation,
    environment: StoredEnvironment,
    timeout_seconds: Option<u64>,
    pending_inputs: BTreeSet<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct StoredEnvironment {
    inherit: BTreeSet<String>,
    values: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
struct StreamNotice {
    operation_id: api::HostOperationId,
    stream: StreamKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StreamKind {
    Audit,
    Output,
}

/// One authenticated input-transfer request accepted by a workspace mux.
#[derive(Debug)]
pub(crate) struct WorkspaceHostOperationInputRequest {
    /// Workspace identity established by the host mux.
    pub workspace: WorkspaceName,
    /// Accepted input-transfer channel.
    pub channel: tascarrel_mux::Channel,
}

struct PendingInputDirectory {
    path: PathBuf,
    retained: bool,
}

impl PendingInputDirectory {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            retained: false,
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn retain(&mut self) {
        self.retained = true;
    }
}

impl Drop for PendingInputDirectory {
    fn drop(&mut self) {
        if !self.retained
            && let Err(error) = fs::remove_dir_all(&self.path)
        {
            tracing::warn!(
                path = %self.path.display(),
                %error,
                "failed to remove incomplete host operation input"
            );
        }
    }
}

/// Caller-relevant host-operation failure categories.
#[derive(Debug, Error)]
pub enum HostOperationServiceError {
    /// The request or current operation state is invalid.
    #[error("invalid host operation request: {0}")]
    InvalidRequest(String),
    /// Trusted workspace configuration is invalid.
    #[error("invalid host command configuration: {0}")]
    InvalidConfiguration(String),
    /// The selected operation does not exist.
    #[error("host operation does not exist")]
    NotFound,
    /// Durable storage or process execution is unavailable.
    #[error("host operation service unavailable: {0}")]
    Unavailable(String),
    /// An internal invariant failed.
    #[error("host operation service failed: {0}")]
    Internal(String),
}

impl HostOperationService {
    /// Opens permanent state and marks processes left running by a previous
    /// daemon instance as interrupted.
    ///
    /// # Errors
    ///
    /// Returns an error when paths are invalid or durable state cannot be
    /// opened, decoded, or recovered.
    #[tracing::instrument(level = "debug", skip_all, err)]
    pub fn open(
        service_config: HostOperationServiceConfig,
    ) -> Result<Self, Report<HostOperationServiceError>> {
        if !service_config.state_directory.is_absolute() || !service_config.git.is_absolute() {
            return Err(invalid_configuration(
                "state directory and Git executable must be absolute",
            ));
        }
        let storage =
            OperationStorage::open(service_config.state_directory).map_err(storage_error)?;
        let mut operations = BTreeMap::new();
        for mut stored in storage.load().map_err(storage_error)? {
            let id = stored.operation.id.clone();
            if operations.contains_key(&id) {
                return Err(internal("duplicate durable host operation identifier"));
            }
            if matches!(
                stored.operation.state,
                api::HostOperationState::Starting(_) | api::HostOperationState::Running(_)
            ) {
                stored.operation.state =
                    api::HostOperationState::Interrupted(api::HostOperationFinished {
                        finished_at: Timestamp::now(),
                        exit_code: None,
                        signal: None,
                    });
                storage.write_record(&stored).map_err(storage_error)?;
                let sequence = next_audit_sequence(&storage, &id)?;
                storage
                    .append_audit(
                        id.0.as_ref(),
                        &api::HostOperationAuditEntry {
                            sequence,
                            timestamp: Timestamp::now(),
                            kind: api::HostOperationAuditKind::Interrupted,
                            message: "hostd restarted while the process was active".into(),
                            actor: Some(Actor::Host),
                        },
                    )
                    .map_err(storage_error)?;
            }
            operations.insert(id, stored);
        }
        let (generation, _) = watch::channel(1);
        let (stream_events, _) = broadcast::channel(1024);
        Ok(Self {
            inner: Arc::new(HostOperationServiceInner {
                git: service_config.git,
                storage,
                operations: Mutex::new(operations),
                generation,
                stream_events,
                running: Mutex::new(HashMap::new()),
                state_transitions: Mutex::new(()),
                audit_writes: Mutex::new(()),
            }),
        })
    }

    /// Creates one immutable operation from the current trusted command
    /// definition.
    ///
    /// # Errors
    ///
    /// Returns an error when the caller, command definition, parameters, or
    /// durable operation state is invalid or unavailable.
    #[allow(clippy::too_many_lines)] // Capturing one immutable plan keeps request-time validation atomic.
    #[tracing::instrument(
        level = "info",
        skip_all,
        fields(workspace = %input.workspace, command = %input.command),
        err
    )]
    pub async fn request(
        &self,
        input: api::RequestHostOperationAction,
        requested_by: Actor,
        config_service: &crate::services::config::ConfigService,
    ) -> Result<api::RequestHostOperationOutput, Report<HostOperationServiceError>> {
        let Actor::Pod(address) = &requested_by else {
            return Err(invalid_request("only a pod may request a host operation"));
        };
        if address.workspace != input.workspace {
            return Err(invalid_request(
                "request workspace does not match the authenticated pod",
            ));
        }
        let snapshot = config_service
            .read(&input.workspace)
            .await
            .map_err(|error| unavailable(error.to_string()))?;
        let workspace_config = snapshot.config.ok_or_else(|| {
            invalid_configuration(snapshot.last_config_error.map_or_else(
                || "workspace config is unavailable".to_owned(),
                |e| e.message.to_string(),
            ))
        })?;
        let definitions = workspace_config
            .host_commands
            .as_ref()
            .ok_or_else(|| invalid_request("workspace defines no host commands"))?;
        let definition = definitions
            .get(input.command.as_ref())
            .ok_or_else(|| invalid_request("host command is not configured"))?;
        validate_command(input.command.as_ref(), definition, &workspace_config)?;
        let parameters = resolve_parameters(definition, &input.parameters)?;

        let id = api::HostOperationId::generate();
        let operation_dir = self.inner.storage.operation_dir(id.0.as_ref());
        let work_dir = operation_dir.join("work");
        let input_paths = definition
            .inputs
            .as_ref()
            .map(|inputs| {
                inputs
                    .keys()
                    .map(|name| {
                        (
                            name.to_string(),
                            self.inner
                                .storage
                                .input_dir(id.0.as_ref(), name.as_ref())
                                .join("tree"),
                        )
                    })
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        let program = resolve_executable(&definition.program)?;
        let arguments = definition
            .arguments
            .as_deref()
            .unwrap_or_default()
            .iter()
            .map(|argument| expand_argument(argument, &parameters, &input_paths))
            .collect::<Result<Vec<_>, _>>()?;
        let working_directory = definition
            .working_directory
            .as_deref()
            .map(|value| expand_working_directory(value, &work_dir, &parameters, &input_paths))
            .transpose()?
            .unwrap_or(work_dir);
        let environment = resolve_environment(definition);
        let environment_names = environment
            .inherit
            .iter()
            .chain(environment.values.keys())
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(Into::into)
            .collect::<Vec<_>>();
        let (inputs, pending_inputs) = resolve_inputs(definition)?;
        let state = if pending_inputs.is_empty() {
            api::HostOperationState::AwaitingApproval(api::HostOperationAwaitingApproval {
                postponed: false,
            })
        } else {
            api::HostOperationState::Preparing
        };
        let operation = api::HostOperation {
            id: id.clone(),
            workspace: input.workspace.clone(),
            pod_id: address.pod_id.clone(),
            command: input.command.clone(),
            description: definition.description.clone(),
            requested_by: requested_by.clone(),
            created_at: Timestamp::now(),
            state,
            parameters: parameters
                .iter()
                .map(|(name, value)| (name.as_str().into(), value.as_str().into()))
                .collect(),
            program: program.to_string_lossy().into_owned().into(),
            arguments: arguments
                .into_iter()
                .map(Into::into)
                .collect::<Vec<_>>()
                .into(),
            working_directory: Some(working_directory.to_string_lossy().into_owned().into()),
            environment_names: environment_names.into(),
            inputs: inputs.into(),
        };
        let stored = StoredOperation {
            operation,
            environment,
            timeout_seconds: definition.timeout_seconds,
            pending_inputs,
        };
        self.inner
            .storage
            .prepare_operation(id.0.as_ref())
            .map_err(storage_error)?;
        self.inner
            .storage
            .write_record(&stored)
            .map_err(storage_error)?;
        let pending = pending_input_list(&stored);
        {
            let mut operations = self.inner.operations.lock().await;
            operations.insert(id.clone(), stored);
        }
        self.append_audit(
            &id,
            api::HostOperationAuditKind::Requested,
            "pod requested the configured host command",
            Some(requested_by),
        )
        .await?;
        self.publish_operations();
        Ok(api::RequestHostOperationOutput {
            operation_id: id,
            inputs: pending.into(),
        })
    }

    /// Applies a host-authorized approval decision.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation is missing, is not awaiting
    /// approval, or its updated state cannot be stored.
    #[tracing::instrument(
        level = "info",
        skip_all,
        fields(operation_id = %input.operation_id.0),
        err
    )]
    pub async fn resolve(
        &self,
        input: api::ResolveHostOperationAction,
        actor: Actor,
    ) -> Result<api::ResolveHostOperationOutput, Report<HostOperationServiceError>> {
        let transition = self.inner.state_transitions.lock().await;
        let mut start = false;
        let (kind, message) = {
            let mut operations = self.inner.operations.lock().await;
            let stored = operations
                .get_mut(&input.operation_id)
                .ok_or_else(|| HostOperationServiceError::NotFound.report())?;
            let mut updated = stored.clone();
            let api::HostOperationState::AwaitingApproval(approval) = &mut updated.operation.state
            else {
                return Err(invalid_request("operation is not awaiting approval"));
            };
            let result = match input.decision {
                api::HostOperationDecision::Postpone => {
                    approval.postponed = true;
                    (
                        api::HostOperationAuditKind::Postponed,
                        "approval was postponed",
                    )
                }
                api::HostOperationDecision::Reject => {
                    updated.operation.state =
                        api::HostOperationState::Rejected(api::HostOperationFinished {
                            finished_at: Timestamp::now(),
                            exit_code: None,
                            signal: None,
                        });
                    (
                        api::HostOperationAuditKind::Rejected,
                        "operation was rejected",
                    )
                }
                api::HostOperationDecision::Approve => {
                    updated.operation.state =
                        api::HostOperationState::Starting(api::HostOperationStarted {
                            approved_by: actor.clone(),
                            approved_at: Timestamp::now(),
                            pid: None,
                        });
                    start = true;
                    (
                        api::HostOperationAuditKind::Approved,
                        "operation was approved",
                    )
                }
            };
            self.inner
                .storage
                .write_record(&updated)
                .map_err(storage_error)?;
            *stored = updated;
            result
        };
        self.append_audit(&input.operation_id, kind, message, Some(actor))
            .await?;
        self.publish_operations();
        drop(transition);
        if start {
            let service = self.clone();
            let id = input.operation_id;
            tokio::spawn(async move {
                if let Err(error) = service.run_operation(id.clone()).await {
                    tracing::error!(operation_id = %id.0, %error, "host operation supervision failed");
                    if let Err(failure_error) = service.fail_operation(&id, error.to_string()).await
                    {
                        tracing::error!(
                            operation_id = %id.0,
                            %failure_error,
                            "failed to record host operation supervision failure"
                        );
                    }
                }
            });
        }
        Ok(api::ResolveHostOperationOutput {})
    }

    /// Withdraws a pending operation or terminates a started one.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation is missing, already terminal, or
    /// its cancellation cannot be stored.
    #[tracing::instrument(
        level = "info",
        skip_all,
        fields(operation_id = %input.operation_id.0),
        err
    )]
    pub async fn cancel(
        &self,
        input: api::CancelHostOperationAction,
        actor: Actor,
    ) -> Result<api::CancelHostOperationOutput, Report<HostOperationServiceError>> {
        let _transition = self.inner.state_transitions.lock().await;
        let sender = self
            .inner
            .running
            .lock()
            .await
            .get(&input.operation_id)
            .cloned();
        if let Some(sender) = sender {
            sender.send_replace(true);
            self.append_audit(
                &input.operation_id,
                api::HostOperationAuditKind::CancelRequested,
                "termination was requested",
                Some(actor),
            )
            .await?;
            return Ok(api::CancelHostOperationOutput {});
        }
        {
            let mut operations = self.inner.operations.lock().await;
            let stored = operations
                .get_mut(&input.operation_id)
                .ok_or_else(|| HostOperationServiceError::NotFound.report())?;
            if !matches!(
                stored.operation.state,
                api::HostOperationState::Preparing
                    | api::HostOperationState::AwaitingApproval(_)
                    | api::HostOperationState::Starting(_)
            ) {
                return Err(invalid_request("operation is already terminal"));
            }
            let mut updated = stored.clone();
            updated.operation.state =
                api::HostOperationState::Canceled(api::HostOperationFinished {
                    finished_at: Timestamp::now(),
                    exit_code: None,
                    signal: None,
                });
            self.inner
                .storage
                .write_record(&updated)
                .map_err(storage_error)?;
            *stored = updated;
        }
        self.append_audit(
            &input.operation_id,
            api::HostOperationAuditKind::Canceled,
            "operation was withdrawn before execution",
            Some(actor),
        )
        .await?;
        self.publish_operations();
        Ok(api::CancelHostOperationOutput {})
    }

    /// Opens a filtered durable operation-list subscription.
    #[must_use]
    pub fn subscribe(
        &self,
        input: api::HostOperationListChangedSubscription,
    ) -> HostOperationSubscription {
        HostOperationSubscription {
            service: self.clone(),
            workspace: input.workspace,
            pod_id: input.pod_id,
            cursor: input.cursor,
            generation: self.inner.generation.subscribe(),
            current_pending: true,
        }
    }

    /// Opens a permanent audit replay followed by live events.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation is missing or its audit log cannot
    /// be read.
    pub async fn subscribe_audit(
        &self,
        input: api::HostOperationAuditSubscription,
    ) -> Result<HostOperationAuditSubscription, Report<HostOperationServiceError>> {
        self.require_operation(&input.operation_id).await?;
        let entries = self.read_audit(&input.operation_id).await?;
        Ok(HostOperationAuditSubscription {
            service: self.clone(),
            operation_id: input.operation_id,
            after_sequence: input.after_sequence.unwrap_or(0),
            replay_boundary: entries.last().map_or(0, |entry| entry.sequence),
            caught_up: false,
            receiver: self.inner.stream_events.subscribe(),
        })
    }

    /// Opens a permanent raw-output replay followed by live chunks.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation is missing or its output log cannot
    /// be read.
    pub async fn subscribe_output(
        &self,
        input: api::HostOperationOutputSubscription,
    ) -> Result<HostOperationOutputSubscription, Report<HostOperationServiceError>> {
        self.require_operation(&input.operation_id).await?;
        let chunks = self.read_output(&input.operation_id).await?;
        Ok(HostOperationOutputSubscription {
            service: self.clone(),
            operation_id: input.operation_id,
            after_sequence: input.after_sequence.unwrap_or(0),
            replay_boundary: chunks.last().map_or(0, |chunk| chunk.sequence),
            caught_up: false,
            receiver: self.inner.stream_events.subscribe(),
        })
    }

    /// Returns one operation for authorization checks.
    ///
    /// # Errors
    ///
    /// Returns an error when the operation does not exist.
    pub async fn get(
        &self,
        id: &api::HostOperationId,
    ) -> Result<api::HostOperation, Report<HostOperationServiceError>> {
        self.require_operation(id)
            .await
            .map(|stored| stored.operation)
    }

    /// Serves authenticated repository-input channels until their queue closes.
    pub(crate) async fn serve_workspace_requests(
        &self,
        mut requests: tokio::sync::mpsc::Receiver<WorkspaceHostOperationInputRequest>,
    ) {
        let mut tasks = JoinSet::new();
        loop {
            tokio::select! {
                request = requests.recv() => {
                    let Some(request) = request else { break };
                    let service = self.clone();
                    tasks.spawn(async move {
                        if let Err(error) = service.serve_input(request).await {
                            tracing::warn!(%error, "host operation input transfer failed");
                        }
                    });
                }
                Some(result) = tasks.join_next(), if !tasks.is_empty() => {
                    if let Err(error) = result {
                        tracing::warn!(%error, "host operation input task failed");
                    }
                }
            }
        }
        tasks.abort_all();
    }

    fn publish_operations(&self) {
        let next = self.inner.generation.borrow().wrapping_add(1);
        self.inner.generation.send_replace(next);
    }

    fn publish_stream_notice(&self, operation_id: &api::HostOperationId, stream: StreamKind) {
        if self
            .inner
            .stream_events
            .send(StreamNotice {
                operation_id: operation_id.clone(),
                stream,
            })
            .is_err()
        {
            tracing::trace!(
                operation_id = %operation_id.0,
                ?stream,
                "host operation stream has no active subscribers"
            );
        }
    }

    async fn require_operation(
        &self,
        id: &api::HostOperationId,
    ) -> Result<StoredOperation, Report<HostOperationServiceError>> {
        self.inner
            .operations
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| HostOperationServiceError::NotFound.report())
    }

    async fn append_audit(
        &self,
        id: &api::HostOperationId,
        kind: api::HostOperationAuditKind,
        message: &str,
        actor: Option<Actor>,
    ) -> Result<(), Report<HostOperationServiceError>> {
        let _write = self.inner.audit_writes.lock().await;
        let storage = self.inner.storage.clone();
        let id_value = id.clone();
        let message = message.to_owned();
        tokio::task::spawn_blocking(move || {
            let sequence = next_audit_sequence(&storage, &id_value)?;
            storage
                .append_audit(
                    id_value.0.as_ref(),
                    &api::HostOperationAuditEntry {
                        sequence,
                        timestamp: Timestamp::now(),
                        kind,
                        message: message.into(),
                        actor,
                    },
                )
                .map_err(storage_error)
        })
        .await
        .map_err(|error| internal(format!("audit storage task failed: {error}")))??;
        self.publish_stream_notice(id, StreamKind::Audit);
        Ok(())
    }

    async fn read_audit(
        &self,
        id: &api::HostOperationId,
    ) -> Result<Vec<api::HostOperationAuditEntry>, Report<HostOperationServiceError>> {
        let storage = self.inner.storage.clone();
        let id = id.0.to_string();
        tokio::task::spawn_blocking(move || storage.read_audit(&id).map_err(storage_error))
            .await
            .map_err(|error| internal(format!("audit read task failed: {error}")))?
    }

    async fn read_output(
        &self,
        id: &api::HostOperationId,
    ) -> Result<Vec<api::HostOperationOutputChunk>, Report<HostOperationServiceError>> {
        let storage = self.inner.storage.clone();
        let id = id.0.to_string();
        tokio::task::spawn_blocking(move || storage.read_output(&id).map_err(storage_error))
            .await
            .map_err(|error| internal(format!("output read task failed: {error}")))?
    }
}

/// Latest-value operation inventory.
pub struct HostOperationSubscription {
    service: HostOperationService,
    workspace: Option<WorkspaceName>,
    pod_id: Option<PodId>,
    cursor: Option<api::HostOperationRevision>,
    generation: watch::Receiver<u64>,
    current_pending: bool,
}

impl HostOperationSubscription {
    /// Receives the current state and each later changed state.
    ///
    /// # Errors
    ///
    /// Returns an error if the service stops or the inventory revision cannot
    /// be calculated.
    pub async fn recv(
        &mut self,
    ) -> Result<api::HostOperationListChangedEvent, Report<HostOperationServiceError>> {
        loop {
            if self.current_pending {
                self.current_pending = false;
            } else {
                self.generation
                    .changed()
                    .await
                    .map_err(|_| unavailable("host operation service stopped"))?;
            }
            let operations = self.service.inner.operations.lock().await;
            let mut values = operations
                .values()
                .filter(|stored| {
                    self.workspace
                        .as_ref()
                        .is_none_or(|workspace| &stored.operation.workspace == workspace)
                        && self
                            .pod_id
                            .as_ref()
                            .is_none_or(|pod_id| &stored.operation.pod_id == pod_id)
                })
                .map(|stored| stored.operation.clone())
                .collect::<Vec<_>>();
            values.sort_by(|left, right| {
                right
                    .created_at
                    .cmp(&left.created_at)
                    .then_with(|| right.id.cmp(&left.id))
            });
            let value = api::HostOperationList {
                operations: values.into(),
            };
            let revision = operation_revision(&value)?;
            if self.cursor.as_ref() == Some(&revision) {
                continue;
            }
            self.cursor = Some(revision.clone());
            return Ok(api::HostOperationListChangedEvent { revision, value });
        }
    }
}

/// Replay and live structured audit subscription.
pub struct HostOperationAuditSubscription {
    service: HostOperationService,
    operation_id: api::HostOperationId,
    after_sequence: u64,
    replay_boundary: u64,
    caught_up: bool,
    receiver: broadcast::Receiver<StreamNotice>,
}

impl HostOperationAuditSubscription {
    /// Receives the next retained or live audit update.
    ///
    /// # Errors
    ///
    /// Returns an error when retained events cannot be read or the service
    /// stops.
    pub async fn recv(
        &mut self,
    ) -> Result<api::HostOperationAuditEvent, Report<HostOperationServiceError>> {
        loop {
            if let Some(entry) = self
                .service
                .read_audit(&self.operation_id)
                .await?
                .into_iter()
                .find(|entry| {
                    entry.sequence > self.after_sequence
                        && (self.caught_up || entry.sequence <= self.replay_boundary)
                })
            {
                self.after_sequence = entry.sequence;
                return Ok(api::HostOperationAuditEvent {
                    update: api::HostOperationAuditUpdate::Event(entry),
                });
            }
            if !self.caught_up {
                self.caught_up = true;
                return Ok(api::HostOperationAuditEvent {
                    update: api::HostOperationAuditUpdate::CaughtUp(api::HostOperationCaughtUp {
                        sequence: self.replay_boundary,
                    }),
                });
            }
            wait_for_notice(&mut self.receiver, &self.operation_id, StreamKind::Audit).await?;
        }
    }
}

/// Replay and live raw-output subscription.
pub struct HostOperationOutputSubscription {
    service: HostOperationService,
    operation_id: api::HostOperationId,
    after_sequence: u64,
    replay_boundary: u64,
    caught_up: bool,
    receiver: broadcast::Receiver<StreamNotice>,
}

impl HostOperationOutputSubscription {
    /// Receives the next retained or live output update.
    ///
    /// # Errors
    ///
    /// Returns an error when retained chunks cannot be read or the service
    /// stops.
    pub async fn recv(
        &mut self,
    ) -> Result<api::HostOperationOutputEvent, Report<HostOperationServiceError>> {
        loop {
            if let Some(chunk) = self
                .service
                .read_output(&self.operation_id)
                .await?
                .into_iter()
                .find(|chunk| {
                    chunk.sequence > self.after_sequence
                        && (self.caught_up || chunk.sequence <= self.replay_boundary)
                })
            {
                self.after_sequence = chunk.sequence;
                return Ok(api::HostOperationOutputEvent {
                    update: api::HostOperationOutputUpdate::Chunk(chunk),
                });
            }
            if !self.caught_up {
                self.caught_up = true;
                return Ok(api::HostOperationOutputEvent {
                    update: api::HostOperationOutputUpdate::CaughtUp(api::HostOperationCaughtUp {
                        sequence: self.replay_boundary,
                    }),
                });
            }
            wait_for_notice(&mut self.receiver, &self.operation_id, StreamKind::Output).await?;
        }
    }
}

impl HostOperationService {
    #[allow(clippy::too_many_lines)] // Process supervision keeps state, output, and termination ordering together.
    #[tracing::instrument(level = "info", skip_all, fields(operation_id = %id.0), err)]
    async fn run_operation(
        &self,
        id: api::HostOperationId,
    ) -> Result<(), Report<HostOperationServiceError>> {
        let transition = self.inner.state_transitions.lock().await;
        let stored = self.require_operation(&id).await?;
        let api::HostOperationState::Starting(started) = &stored.operation.state else {
            return Err(internal("approved operation is not in starting state"));
        };
        let (cancel, mut cancel_receiver) = watch::channel(false);
        self.inner.running.lock().await.insert(id.clone(), cancel);

        let environment = resolve_execution_environment(&stored.environment)?;
        let mut command = Command::new(stored.operation.program.as_ref());
        command
            .args(stored.operation.arguments.iter().map(AsRef::<str>::as_ref))
            .env_clear()
            .envs(environment)
            .current_dir(
                stored
                    .operation
                    .working_directory
                    .as_deref()
                    .ok_or_else(|| internal("operation has no working directory"))?,
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command.process_group(0);
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                self.inner.running.lock().await.remove(&id);
                return Err(unavailable(format!(
                    "could not start host process: {error}"
                )));
            }
        };
        let pid = child.id();
        {
            let mut operations = self.inner.operations.lock().await;
            let stored = operations
                .get_mut(&id)
                .ok_or_else(|| HostOperationServiceError::NotFound.report())?;
            let mut updated = stored.clone();
            updated.operation.state = api::HostOperationState::Running(api::HostOperationStarted {
                approved_by: started.approved_by.clone(),
                approved_at: started.approved_at,
                pid,
            });
            self.inner
                .storage
                .write_record(&updated)
                .map_err(storage_error)?;
            *stored = updated;
        }
        self.append_audit(
            &id,
            api::HostOperationAuditKind::Started,
            "host process started",
            Some(Actor::Host),
        )
        .await?;
        self.publish_operations();
        drop(transition);

        let counters = Arc::new(Mutex::new(OutputCounters::from_existing(
            &self.read_output(&id).await?,
        )));
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| internal("spawned host process has no stdout pipe"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| internal("spawned host process has no stderr pipe"))?;
        let stdout_task = tokio::spawn(read_process_output(
            self.clone(),
            id.clone(),
            api::HostOperationOutputSource::Stdout,
            stdout,
            counters.clone(),
        ));
        let stderr_task = tokio::spawn(read_process_output(
            self.clone(),
            id.clone(),
            api::HostOperationOutputSource::Stderr,
            stderr,
            counters,
        ));

        let timeout = async {
            if let Some(seconds) = stored.timeout_seconds {
                tokio::time::sleep(Duration::from_secs(seconds)).await;
            } else {
                future::pending::<()>().await;
            }
        };
        tokio::pin!(timeout);
        let mut canceled = false;
        let mut timed_out = false;
        let status = tokio::select! {
            result = child.wait() => {
                result.map_err(|error| unavailable(format!("could not wait for host process: {error}")))
            },
            changed = cancel_receiver.changed() => {
                changed
                    .map_err(|_| internal("host process cancellation channel closed"))?;
                canceled = true;
                terminate_process_group(&mut child)
                    .await
                    .map_err(|error| unavailable(format!("could not terminate host process: {error}")))
            }
            () = &mut timeout => {
                timed_out = true;
                terminate_process_group(&mut child)
                    .await
                    .map_err(|error| unavailable(format!("could not terminate timed-out host process: {error}")))
            }
        }?;
        for task in [stdout_task, stderr_task] {
            task.await
                .map_err(|error| internal(format!("output reader task failed: {error}")))??;
        }
        self.inner.running.lock().await.remove(&id);

        let code = status.code();
        let signal = status.signal();
        let (state, kind, message) = if canceled {
            (
                api::HostOperationState::Canceled(api::HostOperationFinished {
                    finished_at: Timestamp::now(),
                    exit_code: code,
                    signal,
                }),
                api::HostOperationAuditKind::Canceled,
                "host process was stopped".to_owned(),
            )
        } else if timed_out {
            (
                api::HostOperationState::Failed(api::HostOperationFailure {
                    message: "host command exceeded its configured timeout".into(),
                    failed_at: Timestamp::now(),
                    exit_code: code,
                    signal,
                }),
                api::HostOperationAuditKind::TimedOut,
                "host process exceeded its timeout".to_owned(),
            )
        } else if status.success() {
            (
                api::HostOperationState::Succeeded(api::HostOperationFinished {
                    finished_at: Timestamp::now(),
                    exit_code: code,
                    signal,
                }),
                api::HostOperationAuditKind::Succeeded,
                "host process exited successfully".to_owned(),
            )
        } else {
            let diagnostic = code.map_or_else(
                || {
                    signal.map_or_else(
                        || "without an exit status".to_owned(),
                        |s| format!("after signal {s}"),
                    )
                },
                |code| format!("with exit code {code}"),
            );
            (
                api::HostOperationState::Failed(api::HostOperationFailure {
                    message: format!("host process exited {diagnostic}").into(),
                    failed_at: Timestamp::now(),
                    exit_code: code,
                    signal,
                }),
                api::HostOperationAuditKind::Failed,
                format!("host process exited {diagnostic}"),
            )
        };
        let _transition = self.inner.state_transitions.lock().await;
        {
            let mut operations = self.inner.operations.lock().await;
            let stored = operations
                .get_mut(&id)
                .ok_or_else(|| HostOperationServiceError::NotFound.report())?;
            let mut updated = stored.clone();
            updated.operation.state = state;
            self.inner
                .storage
                .write_record(&updated)
                .map_err(storage_error)?;
            *stored = updated;
        }
        self.append_audit(&id, kind, &message, Some(Actor::Host))
            .await?;
        self.publish_operations();
        Ok(())
    }

    async fn fail_operation(
        &self,
        id: &api::HostOperationId,
        message: String,
    ) -> Result<(), Report<HostOperationServiceError>> {
        let _transition = self.inner.state_transitions.lock().await;
        self.inner.running.lock().await.remove(id);
        {
            let mut operations = self.inner.operations.lock().await;
            let stored = operations
                .get_mut(id)
                .ok_or_else(|| HostOperationServiceError::NotFound.report())?;
            if is_terminal(&stored.operation.state) {
                return Ok(());
            }
            let mut updated = stored.clone();
            updated.operation.state = api::HostOperationState::Failed(api::HostOperationFailure {
                message: message.clone().into(),
                failed_at: Timestamp::now(),
                exit_code: None,
                signal: None,
            });
            self.inner
                .storage
                .write_record(&updated)
                .map_err(storage_error)?;
            *stored = updated;
        }
        self.append_audit(
            id,
            api::HostOperationAuditKind::Failed,
            &message,
            Some(Actor::Host),
        )
        .await?;
        self.publish_operations();
        Ok(())
    }

    async fn append_output(
        &self,
        id: &api::HostOperationId,
        source: api::HostOperationOutputSource,
        data: Vec<u8>,
        counters: &Mutex<OutputCounters>,
    ) -> Result<(), Report<HostOperationServiceError>> {
        // Keep sequence allocation and durable append under the same lock so
        // concurrently-read stdout and stderr cannot be stored out of order.
        let mut counters = counters.lock().await;
        counters.sequence += 1;
        let (start_offset, end_offset) = match source {
            api::HostOperationOutputSource::Stdout => {
                let start = counters.stdout_offset;
                counters.stdout_offset += data.len() as u64;
                (start, counters.stdout_offset)
            }
            api::HostOperationOutputSource::Stderr => {
                let start = counters.stderr_offset;
                counters.stderr_offset += data.len() as u64;
                (start, counters.stderr_offset)
            }
        };
        let chunk = api::HostOperationOutputChunk {
            sequence: counters.sequence,
            source,
            start_offset,
            end_offset,
            timestamp: Timestamp::now(),
            data: ProcessTerminalData::from(data),
        };
        let storage = self.inner.storage.clone();
        let id_value = id.0.to_string();
        let stored_chunk = chunk.clone();
        tokio::task::spawn_blocking(move || {
            storage
                .append_output(&id_value, &stored_chunk)
                .map_err(storage_error)
        })
        .await
        .map_err(|error| internal(format!("output storage task failed: {error}")))??;
        drop(counters);
        self.publish_stream_notice(id, StreamKind::Output);
        Ok(())
    }

    #[allow(clippy::too_many_lines)] // The upload handshake keeps authentication and materialization fail-closed.
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(workspace = %request.workspace),
        err
    )]
    async fn serve_input(
        &self,
        request: WorkspaceHostOperationInputRequest,
    ) -> Result<(), Report<HostOperationServiceError>> {
        let mut framed = Framed::new(request.channel);
        let Some(header) = framed
            .read::<HostOperationInputRequest>()
            .await
            .map_err(|error| invalid_request(format!("invalid input transfer header: {error}")))?
        else {
            return Err(invalid_request("input transfer closed before its header"));
        };
        let input = &header.input;
        let operation_id = api::HostOperationId::from_str(&input.operation_id)
            .map_err(|error| invalid_request(error.to_string()))?;
        if input.length == 0 || input.length > MAX_HOST_OPERATION_INPUT_BYTES {
            write_input_error(
                &mut framed,
                ErrorCode::InvalidRequest,
                "input bundle length is invalid",
            )
            .await?;
            return Ok(());
        }
        let admission = {
            let operations = self.inner.operations.lock().await;
            match operations.get(&operation_id) {
                None => Err((ErrorCode::InvalidRequest, "host operation does not exist")),
                Some(stored)
                    if stored.operation.workspace != request.workspace
                        || stored.operation.pod_id.0.as_ref() != header.pod_id.0 =>
                {
                    Err((
                        ErrorCode::PermissionDenied,
                        "host operation does not belong to this pod",
                    ))
                }
                Some(stored)
                    if !matches!(stored.operation.state, api::HostOperationState::Preparing)
                        || !stored.pending_inputs.contains(&input.input_name) =>
                {
                    Err((
                        ErrorCode::InvalidRequest,
                        "host operation input is not pending",
                    ))
                }
                Some(stored) => Ok(stored.clone()),
            }
        };
        let pending = match admission {
            Ok(pending) => pending,
            Err((code, message)) => {
                write_input_error(&mut framed, code, message).await?;
                return Ok(());
            }
        };
        let Some(expected) = pending
            .operation
            .inputs
            .iter()
            .find(|candidate| candidate.name.as_ref() == input.input_name)
            .cloned()
        else {
            return Err(internal("pending input is absent from operation record"));
        };
        if !valid_git_object(&input.revision)
            || input
                .base_revision
                .as_deref()
                .is_some_and(|revision| !valid_git_object(revision))
        {
            write_input_error(
                &mut framed,
                ErrorCode::InvalidRequest,
                "input revision is not a Git object identifier",
            )
            .await?;
            return Ok(());
        }
        let directory = self
            .inner
            .storage
            .prepare_input(operation_id.0.as_ref(), &input.input_name)
            .map_err(storage_error)?;
        let mut directory = PendingInputDirectory::new(directory);
        let bundle = directory.path().join("input.bundle");
        let mut file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&bundle)
            .await
            .map_err(|error| unavailable(format!("could not create input bundle: {error}")))?;
        framed
            .write(&HostOperationInputResponse::Ready)
            .await
            .map_err(|error| unavailable(format!("could not accept input transfer: {error}")))?;
        let mut channel = framed.into_inner();
        let copied = tokio::io::copy(&mut (&mut channel).take(input.length), &mut file)
            .await
            .map_err(|error| unavailable(format!("could not retain input bundle: {error}")))?;
        file.sync_all()
            .await
            .map_err(|error| unavailable(format!("could not sync input bundle: {error}")))?;
        drop(file);
        let mut framed = Framed::new(channel);
        if copied != input.length {
            write_input_error(
                &mut framed,
                ErrorCode::ExecutionFailed,
                "input transfer ended before the declared bundle length",
            )
            .await?;
            return Ok(());
        }
        let revision = input.revision.clone();
        let base_revision = input.base_revision.clone();
        let materialized = self
            .materialize_input(
                directory.path(),
                &bundle,
                &revision,
                base_revision.as_deref(),
            )
            .await;
        let summary = match materialized {
            Ok(summary) => summary,
            Err(error) => {
                write_input_error(&mut framed, ErrorCode::InvalidRequest, &error.to_string())
                    .await?;
                self.fail_operation(&operation_id, error.to_string())
                    .await?;
                return Ok(());
            }
        };
        let transition = self.inner.state_transitions.lock().await;
        {
            let mut operations = self.inner.operations.lock().await;
            let stored = operations
                .get_mut(&operation_id)
                .ok_or_else(|| HostOperationServiceError::NotFound.report())?;
            if !matches!(stored.operation.state, api::HostOperationState::Preparing)
                || !stored.pending_inputs.contains(&input.input_name)
            {
                drop(operations);
                drop(transition);
                write_input_error(
                    &mut framed,
                    ErrorCode::InvalidRequest,
                    "host operation input is no longer pending",
                )
                .await?;
                return Ok(());
            }
            let mut updated = stored.clone();
            let Some(operation_input) = updated
                .operation
                .inputs
                .iter_mut()
                .find(|candidate| candidate.name == expected.name)
            else {
                return Err(internal("operation input disappeared during transfer"));
            };
            operation_input.revision = Some(revision.into());
            operation_input.base_revision = base_revision.map(Into::into);
            operation_input.materialized_path = Some(
                directory
                    .path()
                    .join("tree")
                    .to_string_lossy()
                    .into_owned()
                    .into(),
            );
            operation_input.change_summary = (!summary.is_empty()).then(|| summary.into());
            updated.pending_inputs.remove(&input.input_name);
            if updated.pending_inputs.is_empty() {
                updated.operation.state =
                    api::HostOperationState::AwaitingApproval(api::HostOperationAwaitingApproval {
                        postponed: false,
                    });
            }
            self.inner
                .storage
                .write_record(&updated)
                .map_err(storage_error)?;
            *stored = updated;
        }
        directory.retain();
        self.append_audit(
            &operation_id,
            api::HostOperationAuditKind::InputCaptured,
            &format!(
                "repository input `{}` was verified and pinned",
                input.input_name
            ),
            Some(Actor::Host),
        )
        .await?;
        self.publish_operations();
        drop(transition);
        framed
            .write(&HostOperationInputResponse::Completed)
            .await
            .map_err(|error| unavailable(format!("could not complete input transfer: {error}")))?;
        Ok(())
    }

    async fn materialize_input(
        &self,
        directory: &Path,
        bundle: &Path,
        revision: &str,
        base_revision: Option<&str>,
    ) -> Result<String, Report<HostOperationServiceError>> {
        let repository = directory.join("repository.git");
        run_git(
            &self.inner.git,
            directory,
            ["init", "--bare", repository.to_string_lossy().as_ref()],
        )
        .await?;
        run_git(
            &self.inner.git,
            directory,
            [
                "--git-dir",
                repository.to_string_lossy().as_ref(),
                "bundle",
                "unbundle",
                bundle.to_string_lossy().as_ref(),
            ],
        )
        .await?;
        run_git(
            &self.inner.git,
            directory,
            [
                "--git-dir",
                repository.to_string_lossy().as_ref(),
                "cat-file",
                "-e",
                &format!("{revision}^{{commit}}"),
            ],
        )
        .await?;
        let tree = directory.join("tree");
        tokio::fs::create_dir(&tree).await.map_err(|error| {
            unavailable(format!("could not create materialized input: {error}"))
        })?;
        run_git(
            &self.inner.git,
            directory,
            [
                "--git-dir",
                repository.to_string_lossy().as_ref(),
                "--work-tree",
                tree.to_string_lossy().as_ref(),
                "checkout",
                "--force",
                revision,
                "--",
                ".",
            ],
        )
        .await?;
        let mut arguments = vec![
            "--git-dir".to_owned(),
            repository.to_string_lossy().into_owned(),
            "diff".to_owned(),
            "--stat".to_owned(),
            "--no-ext-diff".to_owned(),
        ];
        if let Some(base_revision) = base_revision {
            arguments.push(base_revision.to_owned());
        } else {
            arguments.push(format!("{revision}^"));
        }
        arguments.push(revision.to_owned());
        match git_output(&self.inner.git, directory, &arguments).await {
            Ok(output) => Ok(output.trim().to_owned()),
            Err(error) => {
                tracing::warn!(%error, "failed to summarize host operation repository input");
                Ok(String::new())
            }
        }
    }
}

#[derive(Default)]
struct OutputCounters {
    sequence: u64,
    stdout_offset: u64,
    stderr_offset: u64,
}

impl OutputCounters {
    fn from_existing(chunks: &[api::HostOperationOutputChunk]) -> Self {
        let mut counters = Self::default();
        for chunk in chunks {
            counters.sequence = counters.sequence.max(chunk.sequence);
            match chunk.source {
                api::HostOperationOutputSource::Stdout => {
                    counters.stdout_offset = counters.stdout_offset.max(chunk.end_offset);
                }
                api::HostOperationOutputSource::Stderr => {
                    counters.stderr_offset = counters.stderr_offset.max(chunk.end_offset);
                }
            }
        }
        counters
    }
}

async fn read_process_output<R: AsyncRead + Unpin>(
    service: HostOperationService,
    id: api::HostOperationId,
    source: api::HostOperationOutputSource,
    mut reader: R,
    counters: Arc<Mutex<OutputCounters>>,
) -> Result<(), Report<HostOperationServiceError>> {
    let mut buffer = vec![0_u8; OUTPUT_CHUNK_BYTES];
    loop {
        let read = reader
            .read(&mut buffer)
            .await
            .map_err(|error| unavailable(format!("could not read host process output: {error}")))?;
        if read == 0 {
            return Ok(());
        }
        service
            .append_output(&id, source.clone(), buffer[..read].to_vec(), &counters)
            .await?;
    }
}

async fn terminate_process_group(
    child: &mut tokio::process::Child,
) -> std::io::Result<std::process::ExitStatus> {
    if let Some(pid) = child.id()
        && let Ok(pid) = i32::try_from(pid)
        && let Err(error) = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGTERM,
        )
    {
        tracing::debug!(%error, pid, "failed to send SIGTERM to host process group");
    }
    if let Ok(status) = tokio::time::timeout(PROCESS_TERMINATION_GRACE, child.wait()).await {
        status
    } else {
        if let Some(pid) = child.id()
            && let Ok(pid) = i32::try_from(pid)
            && let Err(error) = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            )
        {
            tracing::debug!(%error, pid, "failed to send SIGKILL to host process group");
        }
        child.wait().await
    }
}

fn operation_revision(
    value: &api::HostOperationList,
) -> Result<api::HostOperationRevision, Report<HostOperationServiceError>> {
    let encoded = serde_json::to_vec(value)
        .map_err(|error| internal(format!("could not encode operation list: {error}")))?;
    Ok(api::HostOperationRevision::new(format!(
        "{:x}",
        Sha256::digest(encoded)
    )))
}

fn next_audit_sequence(
    storage: &OperationStorage,
    id: &api::HostOperationId,
) -> Result<u64, Report<HostOperationServiceError>> {
    let entries = storage
        .read_audit::<api::HostOperationAuditEntry>(id.0.as_ref())
        .map_err(storage_error)?;
    Ok(entries.last().map_or(1, |entry| entry.sequence + 1))
}

async fn wait_for_notice(
    receiver: &mut broadcast::Receiver<StreamNotice>,
    id: &api::HostOperationId,
    stream: StreamKind,
) -> Result<(), Report<HostOperationServiceError>> {
    loop {
        match receiver.recv().await {
            Ok(notice) if notice.operation_id == *id && notice.stream == stream => return Ok(()),
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => {
                return Err(unavailable("host operation stream closed"));
            }
        }
    }
}

async fn run_git<const N: usize>(
    git: &Path,
    current_dir: &Path,
    arguments: [&str; N],
) -> Result<(), Report<HostOperationServiceError>> {
    git_output(
        git,
        current_dir,
        &arguments
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
    )
    .await
    .map(|_| ())
}

async fn git_output(
    git: &Path,
    current_dir: &Path,
    arguments: &[String],
) -> Result<String, Report<HostOperationServiceError>> {
    let output = Command::new(git)
        .args(arguments)
        .current_dir(current_dir)
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|error| unavailable(format!("could not execute Git: {error}")))?;
    if !output.status.success() {
        let diagnostic = String::from_utf8_lossy(&output.stderr);
        return Err(invalid_request(format!(
            "Git rejected the repository input: {}",
            diagnostic.trim()
        )));
    }
    String::from_utf8(output.stdout).map_err(|_| invalid_request("Git returned non-UTF-8 output"))
}

async fn write_input_error(
    framed: &mut Framed<tascarrel_mux::Channel>,
    code: ErrorCode,
    message: &str,
) -> Result<(), Report<HostOperationServiceError>> {
    framed
        .write(&HostOperationInputResponse::Error {
            error: RemoteError::new(code, message),
        })
        .await
        .map_err(|error| unavailable(format!("could not reject input transfer: {error}")))
}

fn valid_git_object(value: &str) -> bool {
    (40..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn is_terminal(state: &api::HostOperationState) -> bool {
    matches!(
        state,
        api::HostOperationState::Succeeded(_)
            | api::HostOperationState::Failed(_)
            | api::HostOperationState::Rejected(_)
            | api::HostOperationState::Canceled(_)
            | api::HostOperationState::Interrupted(_)
    )
}

fn storage_error(report: Report<storage::StorageError>) -> Report<HostOperationServiceError> {
    report.escalate(HostOperationServiceError::Unavailable(
        "durable storage failed".to_owned(),
    ))
}

fn invalid_request(message: impl Into<String>) -> Report<HostOperationServiceError> {
    HostOperationServiceError::InvalidRequest(message.into()).report()
}

fn invalid_configuration(message: impl Into<String>) -> Report<HostOperationServiceError> {
    HostOperationServiceError::InvalidConfiguration(message.into()).report()
}

fn unavailable(message: impl Into<String>) -> Report<HostOperationServiceError> {
    HostOperationServiceError::Unavailable(message.into()).report()
}

fn internal(message: impl Into<String>) -> Report<HostOperationServiceError> {
    HostOperationServiceError::Internal(message.into()).report()
}

#[cfg(test)]
mod tests {
    use std::process::Command as StdCommand;

    use tascarrel_api::types::protocol::PodAddress;
    use tempfile::tempdir;

    use super::*;
    use crate::services::config::ConfigService;
    use crate::services::config::ConfigServiceConfig;

    /// Verifies that input materialization checks out the requested commit.
    #[tokio::test]
    async fn repository_bundle_materializes_the_exact_pinned_commit() {
        let temporary = tempdir().unwrap();
        let repository = temporary.path().join("source");
        fs::create_dir(&repository).unwrap();
        test_git(&repository, ["init"]);
        fs::write(repository.join("deployment.txt"), "pinned working state\n").unwrap();
        test_git(&repository, ["add", "deployment.txt"]);
        test_git(
            &repository,
            [
                "-c",
                "user.name=Tascarrel",
                "-c",
                "user.email=test@tascarrel.invalid",
                "commit",
                "-m",
                "initial",
            ],
        );
        let revision = test_git_output(&repository, ["rev-parse", "HEAD"]);
        let reference = "refs/tascarrel/test/input";
        test_git(&repository, ["update-ref", reference, &revision]);
        let bundle = temporary.path().join("input.bundle");
        test_git(
            &repository,
            ["bundle", "create", bundle.to_str().unwrap(), reference],
        );

        let service = HostOperationService::open(HostOperationServiceConfig::new(
            temporary.path().join("state/host-operations"),
            Path::new("/usr/bin/git"),
        ))
        .unwrap();
        let input = temporary.path().join("materialized");
        fs::create_dir(&input).unwrap();

        service
            .materialize_input(&input, &bundle, &revision, None)
            .await
            .unwrap();

        assert_eq!(
            fs::read_to_string(input.join("tree/deployment.txt")).unwrap(),
            "pinned working state\n"
        );
    }

    /// Verifies command discovery and request admission use hot-reloaded
    /// workspace definitions without exposing environment values.
    #[tokio::test]
    async fn registered_commands_and_requests_follow_config_changes() {
        let temporary = tempdir().unwrap();
        let workspace_root = temporary.path().join("workspaces/demo");
        fs::create_dir_all(workspace_root.join("image")).unwrap();
        let config_path = workspace_root.join("config.toml");
        fs::write(
            &config_path,
            r#"
[host-commands.before]
description = "Initial command"
program = "/bin/true"
arguments = ["${parameters.target}"]
timeout-seconds = 30

[host-commands.before.parameters.target]
default = "staging"
allowed-values = ["staging", "production"]

[host-commands.before.environment]
inherit = ["SSH_AUTH_SOCK"]

[host-commands.before.environment.values]
DEPLOY_MODE = "initial"
"#,
        )
        .unwrap();
        let config = ConfigService::open(ConfigServiceConfig::new(
            temporary.path().join("workspaces"),
        ))
        .unwrap();
        let service = HostOperationService::open(HostOperationServiceConfig::new(
            temporary.path().join("state/host-operations"),
            Path::new("/usr/bin/git"),
        ))
        .unwrap();
        let workspace = WorkspaceName::new("demo");
        let pod_id = PodId::generate();
        let actor = Actor::Pod(PodAddress {
            workspace: workspace.clone(),
            pod_id,
        });
        let mut commands = service
            .subscribe_commands(
                api::HostCommandListChangedSubscription {
                    workspace: workspace.clone(),
                },
                &config,
            )
            .await
            .unwrap();

        let initial = commands.recv().await.unwrap().value;
        assert_eq!(initial.commands.len(), 1);
        let before = &initial.commands[0];
        assert_eq!(before.name.as_ref(), "before");
        assert!(!before.parameters.get("target").unwrap().required);
        assert_eq!(
            before
                .environment_names
                .iter()
                .map(AsRef::<str>::as_ref)
                .collect::<Vec<_>>(),
            ["DEPLOY_MODE", "SSH_AUTH_SOCK"]
        );

        fs::write(
            &config_path,
            r#"
[host-commands.after]
description = "Reloaded command"
program = "/bin/true"
"#,
        )
        .unwrap();
        let reloaded = tokio::time::timeout(Duration::from_secs(5), commands.recv())
            .await
            .unwrap()
            .unwrap()
            .value;
        assert_eq!(reloaded.commands.len(), 1);
        assert_eq!(reloaded.commands[0].name.as_ref(), "after");
        assert!(reloaded.configuration_error.is_none());

        let requested =
            request_test_operation(&service, &config, &workspace, &actor, "after").await;
        assert_eq!(
            service.get(&requested).await.unwrap().command.as_ref(),
            "after"
        );

        fs::write(&config_path, "[host-commands.after\n").unwrap();
        let invalid = tokio::time::timeout(Duration::from_secs(5), commands.recv())
            .await
            .unwrap()
            .unwrap()
            .value;
        assert_eq!(invalid.commands[0].name.as_ref(), "after");
        assert!(invalid.configuration_error.is_some());
    }

    /// Verifies execution, output retention, failure, cancellation, and replay.
    #[tokio::test]
    #[allow(clippy::too_many_lines)] // One scenario verifies success, failure, cancellation, and permanent replay.
    async fn approved_commands_run_and_retain_output_and_terminal_state() {
        let temporary = tempdir().unwrap();
        let workspace_root = temporary.path().join("workspaces/demo");
        fs::create_dir_all(workspace_root.join("image")).unwrap();
        fs::write(
            workspace_root.join("config.toml"),
            r#"
[host-commands.success]
program = "/bin/sh"
arguments = ["-c", "printf 'standard output'; printf 'standard error' >&2"]

[host-commands.failure]
program = "/bin/sh"
arguments = ["-c", "printf 'failed output'; exit 7"]

[host-commands.cancel]
program = "/bin/sh"
arguments = ["-c", "printf 'started'; sleep 30"]
"#,
        )
        .unwrap();
        let config = ConfigService::open(ConfigServiceConfig::new(
            temporary.path().join("workspaces"),
        ))
        .unwrap();
        let service = HostOperationService::open(HostOperationServiceConfig::new(
            temporary.path().join("state/host-operations"),
            Path::new("/usr/bin/git"),
        ))
        .unwrap();
        let workspace = WorkspaceName::new("demo");
        let pod_id = PodId::generate();
        let actor = Actor::Pod(PodAddress {
            workspace: workspace.clone(),
            pod_id,
        });

        let success =
            request_test_operation(&service, &config, &workspace, &actor, "success").await;
        approve_test_operation(&service, success.clone()).await;
        let succeeded = wait_for_terminal(&service, &success).await;
        assert!(matches!(
            succeeded.state,
            api::HostOperationState::Succeeded(_)
        ));
        let chunks = service.read_output(&success).await.unwrap();
        assert_eq!(
            chunks
                .iter()
                .find(|chunk| matches!(chunk.source, api::HostOperationOutputSource::Stdout))
                .unwrap()
                .data
                .as_bytes(),
            b"standard output"
        );
        assert_eq!(
            chunks
                .iter()
                .find(|chunk| matches!(chunk.source, api::HostOperationOutputSource::Stderr))
                .unwrap()
                .data
                .as_bytes(),
            b"standard error"
        );

        let failure =
            request_test_operation(&service, &config, &workspace, &actor, "failure").await;
        approve_test_operation(&service, failure.clone()).await;
        let failed = wait_for_terminal(&service, &failure).await;
        assert!(matches!(
            failed.state,
            api::HostOperationState::Failed(api::HostOperationFailure {
                exit_code: Some(7),
                ..
            })
        ));

        let canceled =
            request_test_operation(&service, &config, &workspace, &actor, "cancel").await;
        approve_test_operation(&service, canceled.clone()).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if matches!(
                    service.get(&canceled).await.unwrap().state,
                    api::HostOperationState::Running(_)
                ) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        service
            .cancel(
                api::CancelHostOperationAction {
                    operation_id: canceled.clone(),
                },
                Actor::Host,
            )
            .await
            .unwrap();
        assert!(matches!(
            wait_for_terminal(&service, &canceled).await.state,
            api::HostOperationState::Canceled(_)
        ));

        let audit = service.read_audit(&success).await.unwrap();
        assert_eq!(
            audit.iter().map(|entry| entry.kind).collect::<Vec<_>>(),
            [
                api::HostOperationAuditKind::Requested,
                api::HostOperationAuditKind::Approved,
                api::HostOperationAuditKind::Started,
                api::HostOperationAuditKind::Succeeded,
            ]
        );
        assert!(
            service
                .inner
                .storage
                .operation_dir(success.0.as_ref())
                .exists()
        );
    }

    async fn request_test_operation(
        service: &HostOperationService,
        config: &ConfigService,
        workspace: &WorkspaceName,
        actor: &Actor,
        command: &str,
    ) -> api::HostOperationId {
        service
            .request(
                api::RequestHostOperationAction {
                    workspace: workspace.clone(),
                    command: command.into(),
                    parameters: HashMap::new(),
                },
                actor.clone(),
                config,
            )
            .await
            .unwrap()
            .operation_id
    }

    async fn approve_test_operation(
        service: &HostOperationService,
        operation_id: api::HostOperationId,
    ) {
        service
            .resolve(
                api::ResolveHostOperationAction {
                    operation_id,
                    decision: api::HostOperationDecision::Approve,
                },
                Actor::Host,
            )
            .await
            .unwrap();
    }

    fn test_git<const N: usize>(repository: &Path, arguments: [&str; N]) {
        test_git_output(repository, arguments);
    }

    fn test_git_output<const N: usize>(repository: &Path, arguments: [&str; N]) -> String {
        let output = StdCommand::new("/usr/bin/git")
            .current_dir(repository)
            .args(arguments)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "Git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    async fn wait_for_terminal(
        service: &HostOperationService,
        operation_id: &api::HostOperationId,
    ) -> api::HostOperation {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let operation = service.get(operation_id).await.unwrap();
                if is_terminal(&operation.state) {
                    return operation;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap()
    }
}
