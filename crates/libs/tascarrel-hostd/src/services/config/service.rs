//! Host-owned workspace configuration snapshots and change observation.
//!
//! [`ConfigService`] reads typed configuration and settings snapshots and owns
//! the native watcher, while [`ConfigSubscription`] exposes its debounced
//! latest-value event stream.

use std::collections::BTreeSet;
use std::collections::HashMap;
use std::io::Write as _;
use std::num::NonZeroUsize;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Weak;
use std::time::Duration;

use notify::Event;
use notify::EventKind;
use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_api::ArcStr;
use tascarrel_api::types::config as api;
use tascarrel_api::types::workspaces::WorkspaceName;
use tascarrel_protocol::WorkspaceName as ValidatedWorkspaceName;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio::time::sleep_until;
use tracing::warn;

use super::settings;
use super::snapshot;
use super::watcher::WatchEvents;
use super::watcher::WatchMessage;

/// Reads workspace configuration inputs and publishes debounced current-state
/// events.
#[derive(Clone)]
pub struct ConfigService {
    inner: Arc<ConfigServiceInner>,
}

impl ConfigService {
    /// Opens the native recursive watcher and starts configuration observation.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigServiceError::InvalidConfiguration`] for invalid service
    /// settings, [`ConfigServiceError::RuntimeUnavailable`] outside a Tokio
    /// runtime, and [`ConfigServiceError::Internal`] when the native watcher
    /// cannot be initialized.
    #[tracing::instrument(level = "debug", skip_all, fields(root = %config.workspaces_directory.display()))]
    pub fn open(config: ConfigServiceConfig) -> Result<Self, Report<ConfigServiceError>> {
        config.validate()?;
        tokio::runtime::Handle::try_current().map_err(|error| {
            error
                .escalate(ConfigServiceError::RuntimeUnavailable)
                .message("start configuration filesystem watcher")
        })?;
        let events = WatchEvents::open(
            &config.workspaces_directory,
            config.watch_channel_capacity.get(),
        )
        .map_err(|report| {
            report
                .escalate(ConfigServiceError::Internal)
                .message("failed to start configuration filesystem watcher")
        })?;
        Ok(Self::start(config, events))
    }

    /// Reads and publishes the current configuration and settings state for one
    /// workspace.
    ///
    /// Failed `config.toml` and `settings.json` loads are represented by their
    /// corresponding error fields. Each input independently retains its last
    /// valid value.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigServiceError::InvalidRequest`] for an invalid workspace
    /// name and [`ConfigServiceError::Unavailable`] when required filesystem
    /// inputs cannot be inspected.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn read(
        &self,
        workspace_name: &WorkspaceName,
    ) -> Result<api::ConfigChangedEvent, Report<ConfigServiceError>> {
        self.inner
            .refresh(workspace_name.clone())
            .await
            .map(|event| (*event).clone())
    }

    /// Atomically replaces one workspace's portable settings and publishes the
    /// resulting configuration event.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigServiceError::InvalidRequest`] for an invalid workspace
    /// name or oversized settings document, [`ConfigServiceError::Unavailable`]
    /// when the workspace input cannot be written, and
    /// [`ConfigServiceError::Internal`] for serialization or refresh failures.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn update_settings(
        &self,
        input: api::UpdateWorkspaceSettingsAction,
    ) -> Result<api::UpdateWorkspaceSettingsOutput, Report<ConfigServiceError>> {
        self.inner.update_settings(input).await
    }

    /// Opens a latest-value subscription and primes it with current state.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::read`] when the initial state cannot
    /// be inspected.
    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn subscribe(
        &self,
        input: api::ConfigChangedSubscription,
    ) -> Result<ConfigSubscription, Report<ConfigServiceError>> {
        let workspace_name = input.workspace_name;
        self.inner.refresh(workspace_name.clone()).await?;
        let states = self.inner.states.lock().await;
        let Some(state) = states.get(&workspace_name) else {
            return Err(internal(
                "configuration state was not published after a successful refresh",
            ));
        };
        Ok(ConfigSubscription {
            current_pending: true,
            receiver: state.subscribe(),
        })
    }

    /// Starts background observation from a prepared event source.
    fn start(config: ConfigServiceConfig, events: WatchEvents) -> Self {
        let (shutdown, shutdown_receiver) = watch::channel(false);
        let inner = Arc::new(ConfigServiceInner {
            workspaces_directory: config.workspaces_directory,
            max_config_bytes: config.max_config_bytes,
            debounce_duration: config.debounce_duration,
            states: Mutex::new(HashMap::new()),
            refresh_lock: Mutex::new(()),
            shutdown,
        });
        let observed = Arc::downgrade(&inner);
        let task = tokio::spawn(run_watcher(observed, events, shutdown_receiver));
        tokio::spawn(async move {
            if let Err(error) = task.await {
                warn!(%error, "configuration watcher task failed");
            }
        });
        Self { inner }
    }
}

/// Filesystem loading and observation settings for [`ConfigService`].
#[derive(Clone, Debug)]
pub struct ConfigServiceConfig {
    /// Directory containing one configuration directory per workspace.
    pub workspaces_directory: PathBuf,
    /// Maximum accepted byte length of one configuration input file.
    pub max_config_bytes: u64,
    /// Quiet period after the most recent native filesystem notification.
    pub debounce_duration: Duration,
    /// Native event queue capacity before the service falls back to a broad
    /// refresh.
    pub watch_channel_capacity: NonZeroUsize,
}

impl ConfigServiceConfig {
    /// Creates service configuration with a 4 MiB file limit and short debounce
    /// period.
    #[must_use]
    pub fn new(workspaces_directory: impl Into<PathBuf>) -> Self {
        Self {
            workspaces_directory: workspaces_directory.into(),
            max_config_bytes: DEFAULT_MAX_CONFIG_BYTES,
            debounce_duration: DEFAULT_DEBOUNCE_DURATION,
            watch_channel_capacity: DEFAULT_WATCH_CHANNEL_CAPACITY,
        }
    }

    /// Validates invariants required by safe loading and event delivery.
    fn validate(&self) -> Result<(), Report<ConfigServiceError>> {
        if !self.workspaces_directory.is_absolute() {
            return Err(invalid_configuration(
                "workspace configuration directory must be absolute",
            ));
        }
        if self.max_config_bytes == 0 {
            return Err(invalid_configuration(
                "maximum configuration input size must be greater than zero",
            ));
        }
        if self.debounce_duration.is_zero() {
            return Err(invalid_configuration(
                "configuration debounce duration must be greater than zero",
            ));
        }
        let metadata = std::fs::symlink_metadata(&self.workspaces_directory).map_err(|error| {
            ConfigServiceError::InvalidConfiguration
                .report()
                .message("failed to inspect workspace configuration directory")
                .message(error.to_string())
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(invalid_configuration(
                "workspace configuration root must be a real directory",
            ));
        }
        Ok(())
    }
}

/// Latest-value stream returned by [`ConfigService::subscribe`].
pub struct ConfigSubscription {
    current_pending: bool,
    receiver: watch::Receiver<Arc<api::ConfigChangedEvent>>,
}

impl ConfigSubscription {
    /// Receives the current state and then each later debounced state change.
    pub async fn recv(&mut self) -> Option<api::ConfigChangedEvent> {
        if self.current_pending {
            self.current_pending = false;
            return Some((**self.receiver.borrow_and_update()).clone());
        }
        self.receiver.changed().await.ok()?;
        Some((**self.receiver.borrow_and_update()).clone())
    }
}

/// Caller-relevant configuration service failure categories.
#[derive(Debug, Error)]
pub enum ConfigServiceError {
    /// The service was constructed with an invalid path or limit.
    #[error("config service configuration is invalid")]
    InvalidConfiguration,
    /// The requested workspace name violates the host path contract.
    #[error("config service request is invalid")]
    InvalidRequest,
    /// Required workspace configuration inputs cannot currently be inspected.
    #[error("workspace configuration is unavailable")]
    Unavailable,
    /// The asynchronous watcher cannot be started.
    #[error("config service runtime is unavailable")]
    RuntimeUnavailable,
    /// Native observation or background execution failed unexpectedly.
    #[error("config service operation failed")]
    Internal,
}

/// Default maximum byte length of one trusted host configuration input.
pub(crate) const DEFAULT_MAX_CONFIG_BYTES: u64 = tascarrel_api::MAX_WORKSPACE_CONFIG_BYTES;
/// Default number of native notifications retained before broad invalidation.
const DEFAULT_WATCH_CHANNEL_CAPACITY: NonZeroUsize =
    NonZeroUsize::new(256).expect("the default watcher channel capacity is non-zero");
/// Default quiet period after the most recent native notification.
const DEFAULT_DEBOUNCE_DURATION: Duration = Duration::from_millis(250);

/// Shared state and shutdown ownership for the configuration service.
struct ConfigServiceInner {
    workspaces_directory: PathBuf,
    max_config_bytes: u64,
    debounce_duration: Duration,
    states: Mutex<HashMap<WorkspaceName, watch::Sender<Arc<api::ConfigChangedEvent>>>>,
    refresh_lock: Mutex<()>,
    shutdown: watch::Sender<bool>,
}

impl ConfigServiceInner {
    /// Loads and publishes one workspace's latest configuration and settings
    /// state.
    async fn refresh(
        &self,
        workspace_name: WorkspaceName,
    ) -> Result<Arc<api::ConfigChangedEvent>, Report<ConfigServiceError>> {
        let _refresh = self.refresh_lock.lock().await;
        self.refresh_locked(workspace_name).await
    }

    /// Replaces settings only when the caller edited the latest configuration
    /// instance, then publishes a freshly identified event while still holding
    /// the refresh lock.
    async fn update_settings(
        &self,
        input: api::UpdateWorkspaceSettingsAction,
    ) -> Result<api::UpdateWorkspaceSettingsOutput, Report<ConfigServiceError>> {
        let _refresh = self.refresh_lock.lock().await;
        let current = self
            .states
            .lock()
            .await
            .get(&input.workspace_name)
            .map(|state| state.borrow().clone())
            .ok_or_else(|| {
                invalid_request("workspace configuration must be read before updating settings")
            })?;
        if current.config_instance_id != input.config_instance_id {
            return Err(invalid_request(
                "workspace configuration changed; reload settings and try again",
            ));
        }
        if current.last_settings_error.is_some() {
            return Err(invalid_request(
                "settings.json must be corrected before it can be updated",
            ));
        }
        settings::validate(&input.settings)
            .map_err(|report| invalid_request(report.error().to_string()))?;

        let workspace_name = input.workspace_name;
        let workspace = workspace_path(&self.workspaces_directory, &workspace_name)?;
        let max_bytes = self.max_config_bytes;
        tokio::task::spawn_blocking(move || {
            write_settings_file(&workspace, &input.settings, max_bytes)
        })
        .await
        .map_err(|error| internal(format!("settings update task failed: {error}")))??;
        self.refresh_locked(workspace_name).await?;
        Ok(api::UpdateWorkspaceSettingsOutput {})
    }

    /// Loads and publishes state while the caller holds `refresh_lock`.
    async fn refresh_locked(
        &self,
        workspace_name: WorkspaceName,
    ) -> Result<Arc<api::ConfigChangedEvent>, Report<ConfigServiceError>> {
        let workspace = workspace_path(&self.workspaces_directory, &workspace_name)?;
        let previous = self
            .states
            .lock()
            .await
            .get(&workspace_name)
            .map(|state| state.borrow().clone());
        let max_config_bytes = self.max_config_bytes;
        let snapshot =
            tokio::task::spawn_blocking(move || snapshot::load(&workspace, max_config_bytes))
                .await
                .map_err(|error| internal(format!("configuration snapshot task failed: {error}")))?
                .map_err(|report| {
                    report
                        .escalate(ConfigServiceError::Unavailable)
                        .message("failed to inspect workspace configuration inputs")
                })?;
        let event = Arc::new(snapshot.into_event(previous.as_deref()));
        let mut states = self.states.lock().await;
        if let Some(state) = states.get(&workspace_name) {
            state.send_replace(Arc::clone(&event));
        } else {
            let (state, _) = watch::channel(Arc::clone(&event));
            states.insert(workspace_name, state);
        }
        Ok(event)
    }

    /// Captures workspace names that have been read or subscribed to.
    async fn tracked_names(&self) -> BTreeSet<WorkspaceName> {
        self.states.lock().await.keys().cloned().collect()
    }
}

impl Drop for ConfigServiceInner {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
    }
}

/// Coalesces native notifications and refreshes affected tracked workspaces.
async fn run_watcher(
    inner: Weak<ConfigServiceInner>,
    mut events: WatchEvents,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        let first = tokio::select! {
            result = shutdown.changed() => {
                if result.is_err() || *shutdown.borrow() {
                    return;
                }
                continue;
            }
            message = events.recv() => message,
        };
        let Some(first) = first else {
            warn!("configuration filesystem watcher stopped");
            return;
        };
        let Some(observed) = inner.upgrade() else {
            return;
        };
        let mut invalidation = Invalidation::default();
        invalidation.observe(first, &observed.workspaces_directory);
        invalidation.all |= events.take_overflow();
        let mut deadline = Instant::now() + observed.debounce_duration;
        drop(observed);

        loop {
            tokio::select! {
                result = shutdown.changed() => {
                    if result.is_err() || *shutdown.borrow() {
                        return;
                    }
                }
                () = sleep_until(deadline) => break,
                message = events.recv() => {
                    let Some(message) = message else {
                        warn!("configuration filesystem watcher stopped");
                        return;
                    };
                    let Some(observed) = inner.upgrade() else {
                        return;
                    };
                    invalidation.observe(message, &observed.workspaces_directory);
                    invalidation.all |= events.take_overflow();
                    deadline = Instant::now() + observed.debounce_duration;
                }
            }
        }

        let Some(observed) = inner.upgrade() else {
            return;
        };
        let tracked = observed.tracked_names().await;
        let names = if invalidation.all {
            tracked
        } else {
            invalidation
                .workspaces
                .intersection(&tracked)
                .cloned()
                .collect()
        };
        for workspace_name in names {
            if let Err(error) = observed.refresh(workspace_name.clone()).await {
                let name: ArcStr = workspace_name.into();
                warn!(workspace = %name, %error, "failed to refresh workspace configuration");
            }
        }
    }
}

/// Workspace-level invalidation accumulated during one event burst.
#[derive(Default)]
struct Invalidation {
    all: bool,
    workspaces: BTreeSet<WorkspaceName>,
}

impl Invalidation {
    /// Merges one watcher message into the current invalidation.
    fn observe(&mut self, message: WatchMessage, root: &Path) {
        match message {
            WatchMessage::Event(event) => self.observe_event(event, root),
            WatchMessage::Error(error) => {
                warn!(%error, "configuration filesystem watcher reported an error");
                self.all = true;
            }
        }
    }

    /// Classifies all paths from one native event.
    fn observe_event(&mut self, event: Event, root: &Path) {
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        if event.paths.is_empty() {
            self.all = true;
            return;
        }
        for path in event.paths {
            match event_workspace(root, &path) {
                Some(workspace_name) => {
                    self.workspaces.insert(workspace_name);
                }
                None => self.all = true,
            }
        }
    }
}

/// Resolves the validated workspace component affected by one event path.
fn event_workspace(root: &Path, path: &Path) -> Option<WorkspaceName> {
    let relative = path.strip_prefix(root).ok()?;
    let Component::Normal(component) = relative.components().next()? else {
        return None;
    };
    let component = component.to_str()?;
    let workspace_name = ValidatedWorkspaceName::new(component.to_owned()).ok()?;
    Some(WorkspaceName::new(workspace_name.as_str()))
}

/// Resolves a validated API workspace name below the configured root.
fn workspace_path(
    root: &Path,
    workspace_name: &WorkspaceName,
) -> Result<PathBuf, Report<ConfigServiceError>> {
    let name: ArcStr = workspace_name.clone().into();
    let validated = ValidatedWorkspaceName::new(name.to_string())
        .map_err(|error| invalid_request(error.to_string()))?;
    Ok(root.join(validated.as_str()))
}

/// Writes a deterministic JSON document through a same-directory temporary
/// file, then durably replaces `settings.json`.
fn write_settings_file(
    workspace: &Path,
    settings: &api::WorkspaceSettings,
    max_bytes: u64,
) -> Result<(), Report<ConfigServiceError>> {
    let metadata = std::fs::symlink_metadata(workspace).map_err(|error| {
        unavailable("failed to inspect workspace settings directory").message(error.to_string())
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(unavailable(
            "workspace settings directory is not a real directory",
        ));
    }
    let mut encoded = serde_json::to_vec_pretty(settings)
        .map_err(|error| internal(format!("failed to encode settings.json: {error}")))?;
    encoded.push(b'\n');
    if u64::try_from(encoded.len()).map_or(true, |length| length > max_bytes) {
        return Err(invalid_request(format!(
            "settings.json exceeds {max_bytes} bytes"
        )));
    }

    let destination = workspace.join("settings.json");
    let temporary = workspace.join(format!(".settings.json.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&temporary)?;
        file.write_all(&encoded)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, &destination)?;
        std::fs::File::open(workspace)?.sync_all()
    })();
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(unavailable("failed to replace settings.json").message(error.to_string()));
    }
    Ok(())
}

fn invalid_configuration(message: impl Into<String>) -> Report<ConfigServiceError> {
    ConfigServiceError::InvalidConfiguration
        .report()
        .message(message.into())
}

fn invalid_request(message: impl Into<String>) -> Report<ConfigServiceError> {
    ConfigServiceError::InvalidRequest
        .report()
        .message(message.into())
}

fn unavailable(message: impl Into<String>) -> Report<ConfigServiceError> {
    ConfigServiceError::Unavailable
        .report()
        .message(message.into())
}

fn internal(message: impl Into<String>) -> Report<ConfigServiceError> {
    ConfigServiceError::Internal
        .report()
        .message(message.into())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use notify::Event;
    use notify::EventKind;
    use tempfile::tempdir;
    use tokio::sync::mpsc;
    use tokio::time::sleep;
    use tokio::time::timeout;

    use super::*;

    fn workspace(root: &Path, config: &str) -> PathBuf {
        let workspace = root.join("demo");
        fs::create_dir_all(workspace.join("image")).unwrap();
        fs::write(workspace.join("config.toml"), config).unwrap();
        workspace
    }

    /// Verifies reads accept large trusted files and retain the preceding valid
    /// config on error.
    #[tokio::test]
    async fn read_retains_valid_config_after_a_large_file_becomes_invalid() {
        let temporary = tempdir().unwrap();
        let value = "a".repeat(2 * 1024 * 1024);
        let workspace = workspace(temporary.path(), &format!("[env]\nLARGE = {value:?}\n"));
        let service = ConfigService::open(ConfigServiceConfig::new(temporary.path())).unwrap();
        let workspace_name = WorkspaceName::new("demo");

        let valid = service.read(&workspace_name).await.unwrap();
        assert!(valid.config.is_some());
        assert!(valid.last_config_error.is_none());

        fs::write(workspace.join("config.toml"), "[vm\ncores = 4\n").unwrap();
        let invalid = service.read(&workspace_name).await.unwrap();
        assert_eq!(invalid.config, valid.config);
        assert!(invalid.last_config_error.is_some());
    }

    /// Verifies each event burst waits for a fresh quiet debounce period.
    #[tokio::test]
    async fn subscription_debounces_native_event_bursts() {
        let temporary = tempdir().unwrap();
        let workspace = workspace(temporary.path(), "[vm]\ncores = 1\n");
        let (sender, receiver) = mpsc::channel(8);
        let mut config = ConfigServiceConfig::new(temporary.path());
        config.debounce_duration = Duration::from_millis(80);
        config.validate().unwrap();
        let service = ConfigService::start(config, WatchEvents::from_receiver(receiver));
        let mut subscription = service
            .subscribe(api::ConfigChangedSubscription {
                workspace_name: WorkspaceName::new("demo"),
            })
            .await
            .unwrap();
        let initial = subscription.recv().await.unwrap();
        assert_eq!(initial.config.unwrap().vm.unwrap().cores, Some(1));

        fs::write(workspace.join("config.toml"), "[vm]\ncores = 2\n").unwrap();
        sender
            .send(WatchMessage::Event(
                Event::new(EventKind::Any).add_path(workspace.join("config.toml")),
            ))
            .await
            .unwrap();
        sleep(Duration::from_millis(50)).await;
        fs::write(workspace.join("config.toml"), "[vm]\ncores = 3\n").unwrap();
        sender
            .send(WatchMessage::Event(
                Event::new(EventKind::Any).add_path(workspace.join("config.toml")),
            ))
            .await
            .unwrap();

        assert!(
            timeout(Duration::from_millis(50), subscription.recv())
                .await
                .is_err()
        );
        let changed = timeout(Duration::from_millis(100), subscription.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(changed.config.unwrap().vm.unwrap().cores, Some(3));
    }

    /// Verifies the settings action durably replaces the host file and
    /// publishes the typed state before returning.
    #[tokio::test]
    async fn update_settings_publishes_the_written_document() {
        let temporary = tempdir().unwrap();
        let workspace = workspace(temporary.path(), "");
        let service = ConfigService::open(ConfigServiceConfig::new(temporary.path())).unwrap();
        let workspace_name = WorkspaceName::new("demo");
        let mut subscription = service
            .subscribe(api::ConfigChangedSubscription {
                workspace_name: workspace_name.clone(),
            })
            .await
            .unwrap();
        let initial = subscription.recv().await.unwrap();
        assert!(initial.settings.is_none());
        let settings = api::WorkspaceSettings { chat: None };

        service
            .update_settings(api::UpdateWorkspaceSettingsAction {
                workspace_name: workspace_name.clone(),
                config_instance_id: initial.config_instance_id.clone(),
                settings: settings.clone(),
            })
            .await
            .unwrap();

        let updated = subscription.recv().await.unwrap();
        assert_ne!(updated.config_instance_id, initial.config_instance_id);
        assert_eq!(updated.settings, Some(settings.clone()));
        assert_eq!(
            fs::read_to_string(workspace.join("settings.json")).unwrap(),
            "{}\n"
        );

        let stale = service
            .update_settings(api::UpdateWorkspaceSettingsAction {
                workspace_name,
                config_instance_id: initial.config_instance_id,
                settings,
            })
            .await
            .unwrap_err();
        assert!(matches!(stale.error(), ConfigServiceError::InvalidRequest));
    }

    /// Verifies an invalid Tasci catalog is rejected before settings.json is
    /// created or the observed configuration instance changes.
    #[tokio::test]
    async fn update_settings_rejects_dangling_tasci_model_endpoint() {
        let temporary = tempdir().unwrap();
        let workspace = workspace(temporary.path(), "");
        let service = ConfigService::open(ConfigServiceConfig::new(temporary.path())).unwrap();
        let workspace_name = WorkspaceName::new("demo");
        let initial = service.read(&workspace_name).await.unwrap();
        let settings = serde_json::from_str(
            r#"{
                "chat": {
                    "tasci": {
                        "endpoints": {},
                        "models": {
                            "qwen": {
                                "endpoint": "missing",
                                "model": "qwen3.6-35b-a3b-q6"
                            }
                        }
                    }
                }
            }"#,
        )
        .unwrap();

        let error = service
            .update_settings(api::UpdateWorkspaceSettingsAction {
                workspace_name: workspace_name.clone(),
                config_instance_id: initial.config_instance_id.clone(),
                settings,
            })
            .await
            .unwrap_err();

        assert!(matches!(error.error(), ConfigServiceError::InvalidRequest));
        assert!(!workspace.join("settings.json").exists());
        assert!(
            service
                .read(&workspace_name)
                .await
                .unwrap()
                .settings
                .is_none()
        );
    }
}
