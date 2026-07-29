//! Workspace-wide harness installation, credentials, discovery, and bindings.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::io::Read as _;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use flate2::read::GzDecoder;
use futures_util::StreamExt as _;
use futures_util::future::BoxFuture;
use jiff::Timestamp;
use reportify::Report;
use reportify::ResultExt as _;
use sha2::Digest as _;
use sha2::Sha512;
use tar::Archive;
use tascarrel_api::ArcVec;
use tascarrel_api::types::chats as api;
use tascarrel_api::types::config as config_api;
use thiserror::Error;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::watch;
use tokio::time::sleep;

use crate::GuestNetworkService;
use crate::ProcessSupervisor;
use crate::services::chats::adaptors::ClaudeCodeAdaptor;
use crate::services::chats::adaptors::CodexAdaptor;
use crate::services::chats::adaptors::TasciAdaptor;
use crate::services::chats::adaptors::TasciConfigurationStore;
use crate::services::chats::auth::AccountReadResult;
use crate::services::chats::auth::CodexAuthServer;
use crate::services::chats::auth::LoginCompletedParams;
use crate::services::chats::binding::AttachHarnessBindingRequest;
use crate::services::chats::binding::BindingProvider;
use crate::services::chats::binding::HarnessBinding;
use crate::services::chats::binding::HarnessBindingControl;
use crate::services::chats::binding::HarnessBindingError;
use crate::services::chats::binding::HarnessBindingEventStream;
use crate::services::chats::harness::Harness;
use crate::services::chats::harness::HarnessControl;
use crate::services::chats::harness::HarnessEventStream;
use crate::services::chats::harness::protocol::HarnessCommand;
use crate::services::chats::harness::protocol::HarnessCommandResult;
use crate::services::chats::harness::protocol::HarnessEvent;
use crate::services::chats::harness::protocol::StartSessionRequest;
use crate::services::chats::pricing::ModelPricingCatalog;
use crate::services::chats::process::HarnessProcessLauncher;
use crate::services::chats::process::LocalHarnessProcessLauncher;
use crate::services::chats::process::PodHarnessProcessLauncher;
use crate::services::chats::process::ProcessEnvironment;
use crate::services::chats::title::ClaudeExecTitleGenerator;
use crate::services::chats::title::CodexExecTitleGenerator;
use crate::services::chats::title::GenerateTitleRequest;
use crate::services::chats::title::GeneratedTitle;
use crate::services::chats::title::TitleGenerationError;
use crate::services::chats::title::TitleGenerationService;
use crate::services::pods::PodService;

const CODEX_VERSION: &str = "0.144.4";
const CLAUDE_CODE_VERSION: &str = "2.1.220";
const TASCI_VERSION: &str = env!("CARGO_PKG_VERSION");
const INSTALL_RETRY_DELAY: Duration = Duration::from_secs(30);
const INSTALL_DOWNLOAD_TIMEOUT: Duration = Duration::from_mins(2);
const PRICING_RETRY_DELAY: Duration = Duration::from_secs(30);
const PRICING_REFRESH_INTERVAL: Duration = Duration::from_hours(24);
const MAX_ARCHIVE_BYTES: u64 = 400 * 1024 * 1024;
const MAX_SECRET_BYTES: usize = 16 * 1024;
const MAX_SECRET_BYTES_U64: u64 = 16 * 1024;

/// Workspace-wide owner of installed harnesses and their observable state.
pub(crate) struct HarnessManager {
    root: PathBuf,
    harnesses_root: PathBuf,
    pricing_cache: PathBuf,
    codex_credentials: PathBuf,
    claude_credentials: PathBuf,
    tasci_executable: PathBuf,
    harness_uid: u32,
    harness_gid: u32,
    local_launcher: Arc<dyn HarnessProcessLauncher>,
    state: Mutex<ArcVec<api::ChatHarness>>,
    changes: watch::Sender<ArcVec<api::ChatHarness>>,
    pricing: Mutex<Option<ModelPricingCatalog>>,
    pricing_refresh_started: AtomicBool,
    codex_install: AsyncMutex<()>,
    claude_install: AsyncMutex<()>,
    codex_login: AsyncMutex<Option<ActiveCodexLogin>>,
    codex_credentials_revision: AtomicU64,
    claude_credentials_revision: AtomicU64,
    tasci_configurations: TasciConfigurationStore,
}

impl HarnessManager {
    /// Opens workspace chat state and inspects existing pinned installations.
    pub(crate) fn open(
        root: PathBuf,
        harness_user_id: u32,
        harness_group_id: u32,
        tasci_executable: PathBuf,
    ) -> Result<Arc<Self>, Report<HarnessManagerError>> {
        prepare_directory(&root, 0o711).whatever("failed to prepare chat state directory")?;
        let harnesses_root = root.join("harnesses");
        prepare_directory(&harnesses_root, 0o755)
            .whatever("failed to prepare harness installation directory")?;
        let pricing_root = root.join("pricing");
        prepare_directory(&pricing_root, 0o755)
            .whatever("failed to prepare model pricing directory")?;
        let pricing_cache = pricing_root.join("models-dev.json");
        let pricing = match ModelPricingCatalog::load(&pricing_cache) {
            Ok(pricing) => pricing,
            Err(error) => {
                tracing::warn!(error = %error, "failed to load cached chat model pricing");
                None
            }
        };
        let codex_credentials = root.join("harness-codex");
        let claude_credentials = root.join("harness-claude-code");
        prepare_credential_directory(&codex_credentials, harness_user_id, harness_group_id)?;
        prepare_credential_directory(&claude_credentials, harness_user_id, harness_group_id)?;
        write_codex_config(&codex_credentials, harness_user_id, harness_group_id)?;
        prepare_owned_credential_tree(&codex_credentials, harness_user_id, harness_group_id)?;
        prepare_owned_credential_tree(&claude_credentials, harness_user_id, harness_group_id)?;

        let initial: ArcVec<_> = vec![
            initial_tasci_harness(tasci_executable.is_file()),
            initial_harness(
                api::ChatHarnessKind::Codex,
                "Codex",
                CODEX_VERSION,
                codex_executable(&harnesses_root).is_file(),
                credentials_exist(&codex_credentials, &["auth.json"])
                    .whatever("failed to inspect Codex credentials")?,
            ),
            initial_harness(
                api::ChatHarnessKind::ClaudeCode,
                "Claude Code",
                CLAUDE_CODE_VERSION,
                claude_executable(&harnesses_root).is_file(),
                credentials_exist(&claude_credentials, &["setup-token", ".credentials.json"])
                    .whatever("failed to inspect Claude Code credentials")?,
            ),
        ]
        .into();
        let (changes, _) = watch::channel(initial.clone());
        Ok(Arc::new(Self {
            root,
            harnesses_root,
            pricing_cache,
            codex_credentials,
            claude_credentials,
            tasci_executable,
            harness_uid: harness_user_id,
            harness_gid: harness_group_id,
            local_launcher: Arc::new(LocalHarnessProcessLauncher::new(
                harness_user_id,
                harness_group_id,
            )),
            state: Mutex::new(initial),
            changes,
            pricing: Mutex::new(pricing),
            pricing_refresh_started: AtomicBool::new(false),
            codex_install: AsyncMutex::new(()),
            claude_install: AsyncMutex::new(()),
            codex_login: AsyncMutex::new(None),
            codex_credentials_revision: AtomicU64::new(0),
            claude_credentials_revision: AtomicU64::new(0),
            tasci_configurations: TasciConfigurationStore::default(),
        }))
    }

    /// Publishes a host-resolved Tasci model and caches its runtime
    /// configuration for attachments and active sessions.
    pub(crate) fn configure_tasci(&self, output: config_api::ResolveTasciModelOutput) {
        let configuration = self.tasci_configurations.configure(output);
        self.update_harness(&api::ChatHarnessKind::Tasci, |harness| {
            harness.models = configuration.models.clone();
            harness.credentials =
                api::ChatHarnessCredentialState::Valid(api::ChatHarnessValidCredentials {
                    method: "workspace-settings".into(),
                    email: None,
                    plan: None,
                    checked_at: Timestamp::now(),
                });
        });
    }

    /// Removes the published Tasci catalog when workspace settings no longer
    /// contain a valid model selection.
    pub(crate) fn clear_tasci_catalog(&self) {
        self.update_harness(&api::ChatHarnessKind::Tasci, |harness| {
            harness.models = ArcVec::new();
        });
    }

    /// Starts the pricing refresh and eager installation loops.
    pub(crate) fn start_eager_installation(self: &Arc<Self>) {
        self.start_pricing_refresh();
        for kind in [
            api::ChatHarnessKind::Codex,
            api::ChatHarnessKind::ClaudeCode,
        ] {
            let manager = Arc::clone(self);
            tokio::spawn(async move {
                loop {
                    if manager.is_installed(&kind) {
                        break;
                    }
                    if let Err(error) = manager.install(kind.clone()).await {
                        tracing::warn!(harness = ?kind, error = %error, "failed to install chat harness");
                    }
                    if manager.is_installed(&kind) {
                        break;
                    }
                    sleep(INSTALL_RETRY_DELAY).await;
                }
                if manager.credentials_are_present(&kind)
                    && let Err(error) = manager.schedule_credential_validation(kind.clone())
                {
                    tracing::warn!(harness = ?kind, error = %error, "failed to schedule chat harness credential validation");
                }
            });
        }
    }

    /// Returns a complete replacement subscription to workspace harness state.
    pub(crate) fn subscribe(&self) -> HarnessListSubscription {
        HarnessListSubscription {
            receiver: self.changes.subscribe(),
            initial: true,
        }
    }

    /// Installs and verifies one pinned harness version.
    #[tracing::instrument(level = "info", skip(self), fields(harness = ?kind))]
    pub(crate) async fn install(
        &self,
        kind: api::ChatHarnessKind,
    ) -> Result<(), Report<HarnessManagerError>> {
        if kind == api::ChatHarnessKind::Tasci {
            if self.tasci_executable.is_file() {
                return Ok(());
            }
            return Err(Report::new(HarnessManagerError::Internal(
                "the bundled Tasci harness executable is unavailable".to_owned(),
            )));
        }
        let install_guard = match kind {
            api::ChatHarnessKind::Tasci => unreachable!(
                "Tasci installation is handled before selecting a downloadable harness lock"
            ),
            api::ChatHarnessKind::Codex => self.codex_install.lock().await,
            api::ChatHarnessKind::ClaudeCode => self.claude_install.lock().await,
        };
        if self.is_installed(&kind) {
            return Ok(());
        }
        self.update_harness(&kind, |harness| {
            harness.installation = api::ChatHarnessInstallationState::Installing;
        });
        let result = match harness_pin(&kind) {
            Ok(pin) => install_archive(&self.harnesses_root, pin).await,
            Err(report) => Err(report),
        };
        match result {
            Ok(()) => {
                self.update_harness(&kind, |harness| {
                    harness.installation = api::ChatHarnessInstallationState::Installed(
                        api::ChatHarnessInstallation {
                            installed_at: Timestamp::now(),
                        },
                    );
                });
                drop(install_guard);
                self.discover_models(kind).await;
                Ok(())
            }
            Err(report) => {
                let message = report.to_string();
                self.update_harness(&kind, |harness| {
                    harness.installation = api::ChatHarnessInstallationState::Failed(
                        api::ChatHarnessInstallationFailure {
                            code: "installation_failed".into(),
                            message: message.clone().into(),
                            occurred_at: Timestamp::now(),
                        },
                    );
                });
                drop(install_guard);
                Err(report)
            }
        }
    }

    /// Starts a provider-owned authentication operation.
    #[tracing::instrument(level = "info", skip_all)]
    pub(crate) async fn start_auth(
        self: &Arc<Self>,
        request: api::ChatHarnessAuthRequest,
    ) -> Result<(), Report<HarnessManagerError>> {
        match request {
            api::ChatHarnessAuthRequest::CodexDeviceCode => self.start_codex_auth().await,
            api::ChatHarnessAuthRequest::ClaudeSetupToken(request) => {
                self.install(api::ChatHarnessKind::ClaudeCode).await?;
                install_claude_token(
                    &self.claude_credentials,
                    request.token.as_ref(),
                    self.harness_uid,
                    self.harness_gid,
                )?;
                self.advance_credentials_revision(&api::ChatHarnessKind::ClaudeCode);
                self.update_harness(&api::ChatHarnessKind::ClaudeCode, |harness| {
                    harness.credentials = api::ChatHarnessCredentialState::Present;
                    harness.validating_credentials = false;
                    harness.login = api::ChatHarnessLoginState::Idle;
                });
                self.schedule_credential_validation(api::ChatHarnessKind::ClaudeCode)
            }
        }
    }

    /// Starts validation while retaining the last known credential state.
    pub(crate) fn schedule_credential_validation(
        self: &Arc<Self>,
        kind: api::ChatHarnessKind,
    ) -> Result<(), Report<HarnessManagerError>> {
        if kind == api::ChatHarnessKind::Tasci {
            return Err(Report::new(HarnessManagerError::InvalidRequest(
                "Tasci authorization is configured through workspace settings and network policy"
                    .to_owned(),
            )));
        }
        if !self.credentials_are_present(&kind) {
            return Err(Report::new(HarnessManagerError::InvalidRequest(
                "the selected harness has no workspace credentials".to_owned(),
            )));
        }
        let already_running = lock(&self.state)
            .iter()
            .find(|harness| harness.kind == kind)
            .is_some_and(|harness| harness.validating_credentials);
        if already_running {
            return Ok(());
        }
        self.update_harness(&kind, |harness| {
            harness.validating_credentials = true;
        });
        let revision = self.credentials_revision(&kind);
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(error) = manager.validate_credentials(kind.clone(), revision).await {
                tracing::warn!(harness = ?kind, error = %error, "failed to validate chat harness credentials");
            }
        });
        Ok(())
    }

    /// Cancels a provider login that is currently in progress.
    pub(crate) async fn cancel_auth(
        &self,
        kind: api::ChatHarnessKind,
    ) -> Result<(), Report<HarnessManagerError>> {
        if kind != api::ChatHarnessKind::Codex {
            return Err(Report::new(HarnessManagerError::InvalidRequest(
                "the selected harness has no cancellable authentication flow".to_owned(),
            )));
        }
        let active = self.codex_login.lock().await.take();
        if let Some(active) = active {
            if let Err(error) = active.server.cancel_login(&active.login_id).await {
                tracing::warn!(message = %error.message, "failed to cancel Codex login with provider");
            }
            active.server.stop().await.map_err(harness_manager_error)?;
        }
        self.update_harness(&kind, |harness| {
            harness.login = api::ChatHarnessLoginState::Idle;
        });
        Ok(())
    }

    /// Validates existing credentials while retaining their previous state.
    #[tracing::instrument(level = "info", skip(self), fields(harness = ?kind))]
    async fn validate_credentials(
        &self,
        kind: api::ChatHarnessKind,
        revision: u64,
    ) -> Result<(), Report<HarnessManagerError>> {
        if let Err(report) = self.install(kind.clone()).await {
            self.update_harness_at_revision(&kind, revision, |harness| {
                harness.validating_credentials = false;
            });
            return Err(report);
        }
        let validation = match kind {
            api::ChatHarnessKind::Tasci => {
                unreachable!("Tasci credential validation is rejected before scheduling validation")
            }
            api::ChatHarnessKind::Codex => self.validate_codex_credentials().await,
            api::ChatHarnessKind::ClaudeCode => self.validate_claude_credentials().await,
        };
        if self.credentials_revision(&kind) != revision {
            return Ok(());
        }
        match validation {
            Ok((credentials, models)) => {
                self.update_harness_at_revision(&kind, revision, |harness| {
                    harness.validating_credentials = false;
                    harness.credentials = api::ChatHarnessCredentialState::Valid(credentials);
                    harness.models = models;
                });
                Ok(())
            }
            Err(report) => {
                let message = report.to_string();
                self.update_harness_at_revision(&kind, revision, |harness| {
                    harness.validating_credentials = false;
                    harness.credentials =
                        api::ChatHarnessCredentialState::Invalid(api::ChatHarnessAuthFailure {
                            code: "credential_validation_failed".into(),
                            message: message.clone().into(),
                            occurred_at: Timestamp::now(),
                        });
                });
                Err(report)
            }
        }
    }

    /// Removes workspace-owned provider credentials.
    #[tracing::instrument(level = "info", skip(self), fields(harness = ?kind))]
    pub(crate) async fn logout(
        &self,
        kind: api::ChatHarnessKind,
    ) -> Result<(), Report<HarnessManagerError>> {
        if kind == api::ChatHarnessKind::Tasci {
            return Err(Report::new(HarnessManagerError::InvalidRequest(
                "Tasci authorization is configured through workspace settings and network policy"
                    .to_owned(),
            )));
        }
        self.advance_credentials_revision(&kind);
        self.update_harness(&kind, |harness| {
            harness.validating_credentials = false;
        });
        match kind {
            api::ChatHarnessKind::Tasci => {
                unreachable!("Tasci logout is rejected before mutating credential state")
            }
            api::ChatHarnessKind::Codex => {
                if let Some(active) = self.codex_login.lock().await.take() {
                    active.server.stop().await.map_err(harness_manager_error)?;
                }
                if self.is_installed(&kind) {
                    let server = self.codex_auth_server().await?;
                    let result = server.logout().await.map_err(harness_manager_error);
                    let stop = server.stop().await.map_err(harness_manager_error);
                    result.and(stop)?;
                }
                remove_file_if_present(&self.codex_credentials.join("auth.json"))
                    .whatever("failed to remove Codex credentials")?;
            }
            api::ChatHarnessKind::ClaudeCode => {
                for name in ["setup-token", ".credentials.json"] {
                    remove_file_if_present(&self.claude_credentials.join(name))
                        .whatever("failed to remove Claude Code credentials")?;
                }
            }
        }
        self.update_harness(&kind, |harness| {
            harness.credentials = api::ChatHarnessCredentialState::Missing;
            harness.validating_credentials = false;
            harness.login = api::ChatHarnessLoginState::Idle;
            harness.models = ArcVec::new();
        });
        Ok(())
    }

    /// Returns the provider environment for title and discovery processes.
    pub(crate) fn local_environment(
        &self,
        kind: &api::ChatHarnessKind,
    ) -> Arc<dyn ProcessEnvironment> {
        match kind {
            api::ChatHarnessKind::Tasci => Arc::new(HarnessEnvironment::tasci()),
            api::ChatHarnessKind::Codex => {
                Arc::new(HarnessEnvironment::codex(self.codex_credentials.clone()))
            }
            api::ChatHarnessKind::ClaudeCode => {
                Arc::new(HarnessEnvironment::claude(self.claude_credentials.clone()))
            }
        }
    }

    /// Returns the pinned executable on the VM filesystem.
    pub(crate) fn local_executable(&self, kind: &api::ChatHarnessKind) -> PathBuf {
        match kind {
            api::ChatHarnessKind::Tasci => self.tasci_executable.clone(),
            api::ChatHarnessKind::Codex => codex_executable(&self.harnesses_root),
            api::ChatHarnessKind::ClaudeCode => claude_executable(&self.harnesses_root),
        }
    }

    async fn discover_models(&self, kind: api::ChatHarnessKind) {
        if kind == api::ChatHarnessKind::Tasci {
            return;
        }
        if !self.credentials_are_present(&kind) {
            return;
        }
        let harness = self.local_harness(&kind);
        match harness.models().await {
            Ok(models) => {
                let models = self.apply_model_pricing(&kind, models);
                self.update_harness(&kind, |entry| entry.models = models);
            }
            Err(error) => tracing::warn!(
                harness = ?kind,
                message = %error.message,
                "failed to discover chat harness models"
            ),
        }
    }

    fn local_harness(&self, kind: &api::ChatHarnessKind) -> Box<dyn Harness> {
        let executable = self.local_executable(kind);
        let environment = self.local_environment(kind);
        match kind {
            api::ChatHarnessKind::Tasci => {
                let configuration = self
                    .tasci_configurations
                    .default_configuration()
                    .expect("a local Tasci harness is created only after a model was configured");
                Box::new(TasciAdaptor::new(
                    executable,
                    Arc::clone(&self.local_launcher),
                    configuration,
                    self.tasci_configurations.clone(),
                ))
            }
            api::ChatHarnessKind::Codex => Box::new(
                CodexAdaptor::new(executable, Arc::clone(&self.local_launcher))
                    .with_process_environment(environment)
                    .with_working_directory(self.root.clone()),
            ),
            api::ChatHarnessKind::ClaudeCode => Box::new(
                ClaudeCodeAdaptor::new(executable, Arc::clone(&self.local_launcher))
                    .with_harness_version(CLAUDE_CODE_VERSION)
                    .with_process_environment(environment)
                    .with_working_directory(self.root.clone()),
            ),
        }
    }

    async fn start_codex_auth(self: &Arc<Self>) -> Result<(), Report<HarnessManagerError>> {
        self.install(api::ChatHarnessKind::Codex).await?;
        let mut slot = self.codex_login.lock().await;
        if slot.is_some() {
            return Err(Report::new(HarnessManagerError::InvalidRequest(
                "Codex authentication is already in progress".to_owned(),
            )));
        }
        let server = self.codex_auth_server().await?;
        let mut notifications = server.subscribe();
        let challenge = match server.start_device_code().await {
            Ok(challenge) => challenge,
            Err(error) => {
                if let Err(stop_error) = server.stop().await {
                    tracing::warn!(
                        message = %stop_error.message,
                        "failed to stop Codex authentication after login initialization failed"
                    );
                }
                return Err(harness_manager_error(error));
            }
        };
        if !challenge.auth_url.starts_with("https://") {
            if let Err(error) = server.stop().await {
                tracing::warn!(message = %error.message, "failed to stop invalid Codex login process");
            }
            return Err(Report::new(HarnessManagerError::Internal(
                "Codex returned an invalid authorization URL".to_owned(),
            )));
        }
        let login_id = challenge.login_id.clone();
        self.update_harness(&api::ChatHarnessKind::Codex, |harness| {
            harness.login = api::ChatHarnessLoginState::Pending(api::ChatHarnessAuthChallenge {
                login_id: challenge.login_id.into(),
                authorization_url: challenge.auth_url.into(),
                user_code: challenge.user_code.map(Into::into),
                expires_at: None,
            });
        });
        *slot = Some(ActiveCodexLogin {
            login_id: login_id.clone(),
            server: Arc::clone(&server),
        });
        drop(slot);

        let manager = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let notification = match notifications.recv().await {
                    Ok(notification) => notification,
                    Err(error) => {
                        manager
                            .finish_codex_auth_transport_failure(login_id, error.to_string())
                            .await;
                        break;
                    }
                };
                if notification.method != "account/login/completed" {
                    continue;
                }
                match notification.decode::<LoginCompletedParams>() {
                    Ok(completion)
                        if completion
                            .login_id
                            .as_deref()
                            .is_none_or(|candidate| candidate == login_id) =>
                    {
                        manager.finish_codex_auth(login_id, completion).await;
                        break;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        manager
                            .finish_codex_auth_transport_failure(login_id, error.message)
                            .await;
                        break;
                    }
                }
            }
        });
        Ok(())
    }

    async fn finish_codex_auth(
        self: &Arc<Self>,
        login_id: String,
        completion: LoginCompletedParams,
    ) {
        let active = self.take_codex_login(&login_id).await;
        let Some(active) = active else {
            return;
        };
        if completion.success {
            self.advance_credentials_revision(&api::ChatHarnessKind::Codex);
            self.update_harness(&api::ChatHarnessKind::Codex, |harness| {
                harness.credentials = api::ChatHarnessCredentialState::Present;
                harness.validating_credentials = false;
                harness.login = api::ChatHarnessLoginState::Idle;
            });
        } else {
            self.publish_login_failure(
                &api::ChatHarnessKind::Codex,
                "login_failed",
                completion
                    .error
                    .as_deref()
                    .unwrap_or("Codex authentication did not complete"),
            );
        }
        if let Err(error) = active.server.stop().await {
            tracing::warn!(message = %error.message, "failed to stop Codex authentication process");
        }
        if completion.success
            && let Err(error) = self.schedule_credential_validation(api::ChatHarnessKind::Codex)
        {
            tracing::warn!(error = %error, "failed to schedule Codex credential validation after login");
        }
    }

    async fn finish_codex_auth_transport_failure(&self, login_id: String, message: String) {
        let active = self.take_codex_login(&login_id).await;
        let Some(active) = active else {
            return;
        };
        self.publish_login_failure(
            &api::ChatHarnessKind::Codex,
            "login_transport_failed",
            &message,
        );
        if let Err(error) = active.server.stop().await {
            tracing::warn!(message = %error.message, "failed to stop failed Codex login process");
        }
    }

    async fn take_codex_login(&self, login_id: &str) -> Option<ActiveCodexLogin> {
        let mut slot = self.codex_login.lock().await;
        if slot
            .as_ref()
            .is_some_and(|active| active.login_id == login_id)
        {
            slot.take()
        } else {
            None
        }
    }

    async fn codex_auth_server(&self) -> Result<Arc<CodexAuthServer>, Report<HarnessManagerError>> {
        CodexAuthServer::launch(
            self.local_executable(&api::ChatHarnessKind::Codex),
            self.local_environment(&api::ChatHarnessKind::Codex)
                .variables()
                .whatever("failed to prepare Codex authentication environment")?,
            self.root.clone(),
            Arc::clone(&self.local_launcher),
        )
        .await
        .map_err(harness_manager_error)
    }

    async fn validate_codex_credentials(
        &self,
    ) -> Result<
        (api::ChatHarnessValidCredentials, ArcVec<api::ChatModel>),
        Report<HarnessManagerError>,
    > {
        let server = self.codex_auth_server().await?;
        let account = server.read_account().await.map_err(harness_manager_error);
        let stop = server.stop().await.map_err(harness_manager_error);
        stop?;
        let account = account?;
        let credentials = valid_account_credentials(account)?;
        let models = self
            .local_harness(&api::ChatHarnessKind::Codex)
            .models()
            .await
            .map_err(harness_manager_error)?;
        let models = self.apply_model_pricing(&api::ChatHarnessKind::Codex, models);
        Ok((credentials, models))
    }

    async fn validate_claude_credentials(
        &self,
    ) -> Result<
        (api::ChatHarnessValidCredentials, ArcVec<api::ChatModel>),
        Report<HarnessManagerError>,
    > {
        let models = self
            .local_harness(&api::ChatHarnessKind::ClaudeCode)
            .models()
            .await
            .map_err(harness_manager_error)?;
        let models = self.apply_model_pricing(&api::ChatHarnessKind::ClaudeCode, models);
        Ok((
            api::ChatHarnessValidCredentials {
                method: "setup-token".into(),
                email: None,
                plan: None,
                checked_at: Timestamp::now(),
            },
            models,
        ))
    }

    fn publish_login_failure(&self, kind: &api::ChatHarnessKind, code: &str, message: &str) {
        self.update_harness(kind, |harness| {
            harness.login = api::ChatHarnessLoginState::Failed(api::ChatHarnessAuthFailure {
                code: code.into(),
                message: message.into(),
                occurred_at: Timestamp::now(),
            });
        });
    }

    fn is_installed(&self, kind: &api::ChatHarnessKind) -> bool {
        lock(&self.state)
            .iter()
            .find(|harness| &harness.kind == kind)
            .is_some_and(|harness| {
                matches!(
                    harness.installation,
                    api::ChatHarnessInstallationState::Installed(_)
                )
            })
    }

    fn credentials_are_present(&self, kind: &api::ChatHarnessKind) -> bool {
        lock(&self.state)
            .iter()
            .find(|harness| &harness.kind == kind)
            .is_some_and(|harness| {
                !matches!(
                    harness.credentials,
                    api::ChatHarnessCredentialState::Missing
                )
            })
    }

    fn credentials_revision(&self, kind: &api::ChatHarnessKind) -> u64 {
        match kind {
            api::ChatHarnessKind::Tasci => {
                unreachable!("Tasci does not use workspace harness credential revisions")
            }
            api::ChatHarnessKind::Codex => &self.codex_credentials_revision,
            api::ChatHarnessKind::ClaudeCode => &self.claude_credentials_revision,
        }
        .load(Ordering::Acquire)
    }

    fn advance_credentials_revision(&self, kind: &api::ChatHarnessKind) {
        match kind {
            api::ChatHarnessKind::Tasci => {
                unreachable!("Tasci does not use workspace harness credential revisions")
            }
            api::ChatHarnessKind::Codex => &self.codex_credentials_revision,
            api::ChatHarnessKind::ClaudeCode => &self.claude_credentials_revision,
        }
        .fetch_add(1, Ordering::AcqRel);
    }

    fn start_pricing_refresh(self: &Arc<Self>) {
        if self.pricing_refresh_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                match ModelPricingCatalog::fetch().await {
                    Ok(fetched) => {
                        if let Err(error) =
                            ModelPricingCatalog::persist(&manager.pricing_cache, &fetched.bytes)
                                .await
                        {
                            tracing::warn!(error = %error, "failed to cache chat model pricing");
                        }
                        manager.publish_pricing(&fetched.catalog);
                        sleep(PRICING_REFRESH_INTERVAL).await;
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "failed to refresh chat model pricing");
                        sleep(PRICING_RETRY_DELAY).await;
                    }
                }
            }
        });
    }

    fn apply_model_pricing(
        &self,
        kind: &api::ChatHarnessKind,
        models: ArcVec<api::ChatModel>,
    ) -> ArcVec<api::ChatModel> {
        let catalog = lock(&self.pricing).clone();
        let Some(catalog) = catalog else {
            return models;
        };
        let mut models = models.iter().cloned().collect::<Vec<_>>();
        catalog.apply(kind, &mut models);
        models.into()
    }

    fn publish_pricing(&self, catalog: &ModelPricingCatalog) {
        {
            let mut current = lock(&self.pricing);
            if current
                .as_ref()
                .is_some_and(|current| current.version() == catalog.version())
            {
                return;
            }
            *current = Some(catalog.clone());
        }

        let mut state = lock(&self.state);
        let mut next = state.iter().cloned().collect::<Vec<_>>();
        for harness in &mut next {
            let mut models = harness.models.iter().cloned().collect::<Vec<_>>();
            catalog.apply(&harness.kind, &mut models);
            harness.models = models.into();
        }
        let next: ArcVec<_> = next.into();
        *state = next.clone();
        self.changes.send_replace(next);
    }

    fn update_harness_at_revision(
        &self,
        kind: &api::ChatHarnessKind,
        revision: u64,
        update: impl FnOnce(&mut api::ChatHarness),
    ) -> bool {
        let mut state = lock(&self.state);
        if self.credentials_revision(kind) != revision {
            return false;
        }
        let mut next = state.iter().cloned().collect::<Vec<_>>();
        let Some(harness) = next.iter_mut().find(|harness| &harness.kind == kind) else {
            return false;
        };
        update(harness);
        let next: ArcVec<_> = next.into();
        *state = next.clone();
        self.changes.send_replace(next);
        true
    }

    fn update_harness(
        &self,
        kind: &api::ChatHarnessKind,
        update: impl FnOnce(&mut api::ChatHarness),
    ) {
        let mut state = lock(&self.state);
        let mut next = state.iter().cloned().collect::<Vec<_>>();
        if let Some(harness) = next.iter_mut().find(|harness| &harness.kind == kind) {
            update(harness);
        }
        let next: ArcVec<_> = next.into();
        *state = next.clone();
        self.changes.send_replace(next);
    }
}

impl BindingProvider for HarnessManager {
    fn harnesses(&self) -> BoxFuture<'_, Result<ArcVec<api::ChatHarness>, HarnessBindingError>> {
        Box::pin(std::future::ready(Ok(lock(&self.state).clone())))
    }

    fn attach(
        &self,
        request: AttachHarnessBindingRequest,
        processes: ProcessSupervisor,
        pods: PodService,
        network_service: Arc<GuestNetworkService>,
    ) -> BoxFuture<'_, Result<HarnessBinding, HarnessBindingError>> {
        Box::pin(async move {
            self.install(request.resumption.harness.clone())
                .await
                .map_err(manager_binding_error)?;
            let launcher: Arc<dyn HarnessProcessLauncher> = Arc::new(
                PodHarnessProcessLauncher::new(request.pod_id, processes, pods, network_service),
            );
            let harness: Box<dyn Harness> = match request.resumption.harness {
                api::ChatHarnessKind::Tasci => {
                    let selected = request
                        .resumption
                        .model
                        .as_ref()
                        .map(|selection| selection.model.to_string())
                        .or_else(|| {
                            self.tasci_configurations
                                .default_configuration()
                                .map(|configuration| configuration.selection.model.to_string())
                        })
                        .ok_or_else(|| {
                            manager_binding_error(Report::new(HarnessManagerError::InvalidRequest(
                                "Tasci has no resolved default model".to_owned(),
                            )))
                        })?;
                    let configuration = self
                        .tasci_configurations
                        .configuration(&selected)
                        .ok_or_else(|| {
                            manager_binding_error(Report::new(HarnessManagerError::InvalidRequest(
                                "the selected Tasci model was not resolved by hostd".to_owned(),
                            )))
                        })?;
                    Box::new(TasciAdaptor::new(
                        PathBuf::from("/usr/local/bin/tasci-exec"),
                        launcher,
                        configuration,
                        self.tasci_configurations.clone(),
                    ))
                }
                api::ChatHarnessKind::Codex => Box::new(
                    CodexAdaptor::new(pod_codex_executable(), launcher).with_process_environment(
                        Arc::new(HarnessEnvironment::codex(PathBuf::from(
                            "/opt/tascarrel/chat/harness-codex",
                        ))),
                    ),
                ),
                api::ChatHarnessKind::ClaudeCode => Box::new(
                    ClaudeCodeAdaptor::new(pod_claude_executable(), launcher)
                        .with_process_environment(Arc::new(
                            HarnessEnvironment::claude_with_source(
                                PathBuf::from("/opt/tascarrel/chat/harness-claude-code"),
                                self.claude_credentials.clone(),
                            ),
                        )),
                ),
            };
            let session = harness
                .start_session(StartSessionRequest {
                    model: request.resumption.model,
                    resume_cursor: request.resumption.resume_cursor,
                })
                .await
                .map_err(harness_binding_error)?;
            Ok(HarnessBinding {
                control: Arc::new(BindingControl {
                    control: session.control,
                }),
                events: Box::new(BindingEvents {
                    events: session.events,
                }),
            })
        })
    }
}

impl TitleGenerationService for HarnessManager {
    fn generate_title(
        &self,
        request: GenerateTitleRequest,
    ) -> BoxFuture<'_, Result<GeneratedTitle, TitleGenerationError>> {
        Box::pin(async move {
            self.install(request.harness.clone())
                .await
                .map_err(title_manager_error)?;
            let executable = self.local_executable(&request.harness);
            let environment = self.local_environment(&request.harness);
            match request.harness {
                api::ChatHarnessKind::Tasci => Err(TitleGenerationError {
                    code: "unsupported_harness".to_owned(),
                    message: "Tasci title generation is not available yet".to_owned(),
                }),
                api::ChatHarnessKind::Codex => {
                    CodexExecTitleGenerator::new(executable)
                        .with_process_environment(environment)
                        .with_identity(self.harness_uid, self.harness_gid)
                        .generate_title(request)
                        .await
                }
                api::ChatHarnessKind::ClaudeCode => {
                    ClaudeExecTitleGenerator::new(executable)
                        .with_process_environment(environment)
                        .with_identity(self.harness_uid, self.harness_gid)
                        .generate_title(request)
                        .await
                }
            }
        })
    }
}

/// Complete replacement stream for the workspace harness list.
pub(crate) struct HarnessListSubscription {
    receiver: watch::Receiver<ArcVec<api::ChatHarness>>,
    initial: bool,
}

impl HarnessListSubscription {
    /// Receives the next complete harness list.
    pub(crate) async fn recv(&mut self) -> Option<ArcVec<api::ChatHarness>> {
        if !self.initial && self.receiver.changed().await.is_err() {
            return None;
        }
        self.initial = false;
        Some(self.receiver.borrow().clone())
    }
}

/// Failure while managing workspace harness state.
#[derive(Debug, Error)]
pub(crate) enum HarnessManagerError {
    /// The requested harness or platform is unsupported.
    #[error("invalid harness request: {0}")]
    InvalidRequest(String),
    /// Installation or credential management failed.
    #[error("chat harness operation failed: {0}")]
    Internal(String),
}

impl reportify::Whatever for HarnessManagerError {
    fn new() -> Self {
        Self::Internal("chat harness operation failed".to_owned())
    }
}

struct ActiveCodexLogin {
    login_id: String,
    server: Arc<CodexAuthServer>,
}

struct BindingControl {
    control: Arc<dyn HarnessControl>,
}

impl HarnessBindingControl for BindingControl {
    fn apply(
        &self,
        command: HarnessCommand,
    ) -> BoxFuture<'_, Result<HarnessCommandResult, HarnessBindingError>> {
        Box::pin(async move {
            self.control
                .apply(command)
                .await
                .map_err(harness_binding_error)
        })
    }

    fn detach(&self) -> BoxFuture<'_, Result<(), HarnessBindingError>> {
        Box::pin(async move {
            self.control
                .apply(HarnessCommand::Stop)
                .await
                .map(harness_command_stopped)
                .map_err(harness_binding_error)
        })
    }
}

struct BindingEvents {
    events: Box<dyn HarnessEventStream>,
}

impl HarnessBindingEventStream for BindingEvents {
    fn next_event(&mut self) -> BoxFuture<'_, Result<Option<HarnessEvent>, HarnessBindingError>> {
        Box::pin(async move {
            self.events
                .next_event()
                .await
                .map_err(harness_binding_error)
        })
    }
}

struct HarnessEnvironment {
    kind: api::ChatHarnessKind,
    credentials: PathBuf,
    credential_source: PathBuf,
}

impl HarnessEnvironment {
    fn tasci() -> Self {
        Self {
            kind: api::ChatHarnessKind::Tasci,
            credentials: PathBuf::new(),
            credential_source: PathBuf::new(),
        }
    }

    fn codex(credentials: PathBuf) -> Self {
        Self {
            kind: api::ChatHarnessKind::Codex,
            credential_source: credentials.clone(),
            credentials,
        }
    }

    fn claude(credentials: PathBuf) -> Self {
        Self {
            kind: api::ChatHarnessKind::ClaudeCode,
            credential_source: credentials.clone(),
            credentials,
        }
    }

    fn claude_with_source(credentials: PathBuf, credential_source: PathBuf) -> Self {
        Self {
            kind: api::ChatHarnessKind::ClaudeCode,
            credentials,
            credential_source,
        }
    }
}

impl ProcessEnvironment for HarnessEnvironment {
    fn variables(&self) -> io::Result<HashMap<String, String>> {
        let mut environment = HashMap::new();
        match self.kind {
            api::ChatHarnessKind::Tasci => {}
            api::ChatHarnessKind::Codex => {
                let home = self.credentials.to_string_lossy().into_owned();
                environment.insert("CODEX_HOME".to_owned(), home.clone());
                environment.insert("CODEX_SQLITE_HOME".to_owned(), home);
                environment.insert("OPENAI_API_KEY".to_owned(), String::new());
                environment.insert("CODEX_API_KEY".to_owned(), String::new());
                environment.insert("CODEX_ACCESS_TOKEN".to_owned(), String::new());
            }
            api::ChatHarnessKind::ClaudeCode => {
                environment.insert(
                    "CLAUDE_CONFIG_DIR".to_owned(),
                    self.credentials.to_string_lossy().into_owned(),
                );
                environment.insert("ANTHROPIC_API_KEY".to_owned(), String::new());
                environment.insert("ANTHROPIC_AUTH_TOKEN".to_owned(), String::new());
                environment.insert("CLAUDE_CODE_OAUTH_REFRESH_TOKEN".to_owned(), String::new());
                environment.insert("CLAUDE_CODE_OAUTH_SCOPES".to_owned(), String::new());
                match read_credential_file(&self.credential_source.join("setup-token")) {
                    Ok(token) => {
                        environment.insert(
                            "CLAUDE_CODE_OAUTH_TOKEN".to_owned(),
                            token.trim().to_owned(),
                        );
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(environment)
    }
}

struct HarnessPin {
    kind: api::ChatHarnessKind,
    version: &'static str,
    url: String,
    integrity: &'static str,
    archive_prefix: String,
}

async fn install_archive(
    harnesses_root: &Path,
    pin: HarnessPin,
) -> Result<(), Report<HarnessManagerError>> {
    let temporary = tempfile::NamedTempFile::new_in(harnesses_root)
        .whatever("failed to create harness download file")?;
    let path = temporary.path().to_path_buf();
    let mut file = tokio::fs::File::from_std(
        temporary
            .reopen()
            .whatever("failed to reopen harness download file")?,
    );
    let response = reqwest::Client::new()
        .get(&pin.url)
        .timeout(INSTALL_DOWNLOAD_TIMEOUT)
        .send()
        .await
        .whatever("failed to download pinned harness")?
        .error_for_status()
        .whatever("pinned harness download returned an error")?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_ARCHIVE_BYTES)
    {
        return Err(Report::new(HarnessManagerError::Internal(
            "pinned harness archive exceeds the size limit".to_owned(),
        )));
    }
    let mut archive_bytes = 0_u64;
    let mut digest = Sha512::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.whatever("failed while downloading pinned harness")?;
        archive_bytes = archive_bytes
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| {
                Report::new(HarnessManagerError::Internal(
                    "pinned harness archive size overflowed".to_owned(),
                ))
            })?;
        if archive_bytes > MAX_ARCHIVE_BYTES {
            return Err(Report::new(HarnessManagerError::Internal(
                "pinned harness archive exceeds the size limit".to_owned(),
            )));
        }
        digest.update(&chunk);
        file.write_all(&chunk)
            .await
            .whatever("failed to store pinned harness archive")?;
    }
    file.flush()
        .await
        .whatever("failed to flush pinned harness archive")?;
    drop(file);
    let expected = BASE64
        .decode(pin.integrity)
        .whatever("failed to decode pinned harness integrity")?;
    if digest.finalize().as_slice() != expected {
        return Err(Report::new(HarnessManagerError::Internal(
            "pinned harness archive failed integrity verification".to_owned(),
        )));
    }

    let staging = tempfile::Builder::new()
        .prefix("harness-install-")
        .tempdir_in(harnesses_root)
        .whatever("failed to create harness staging directory")?;
    let archive_path = path;
    let staging_path = staging.path().to_path_buf();
    tokio::task::spawn_blocking(move || extract_archive(&archive_path, &staging_path))
        .await
        .whatever("harness extraction task failed")??;
    publish_installation(harnesses_root, staging.path(), &pin)?;
    Ok(())
}

fn extract_archive(archive_path: &Path, staging: &Path) -> Result<(), Report<HarnessManagerError>> {
    let file = fs::File::open(archive_path).whatever("failed to open harness archive")?;
    let decoder = GzDecoder::new(file);
    Archive::new(decoder)
        .unpack(staging)
        .whatever("failed to extract harness archive")
}

fn publish_installation(
    harnesses_root: &Path,
    staging: &Path,
    pin: &HarnessPin,
) -> Result<(), Report<HarnessManagerError>> {
    let harness_name = harness_directory_name(&pin.kind);
    let harness_root = harnesses_root.join(harness_name);
    prepare_directory(&harness_root, 0o755).whatever("failed to prepare harness directory")?;
    let version_root = harness_root.join(pin.version);
    let executable = match pin.kind {
        api::ChatHarnessKind::Tasci => {
            return Err(Report::new(HarnessManagerError::InvalidRequest(
                "Tasci is bundled and cannot be installed from an archive".to_owned(),
            )));
        }
        api::ChatHarnessKind::Codex => version_root.join("bin/codex"),
        api::ChatHarnessKind::ClaudeCode => version_root.join("bin/claude"),
    };
    if version_root.exists() && !executable.is_file() {
        fs::remove_dir_all(&version_root)
            .whatever("failed to remove an incomplete harness installation")?;
    }
    if !version_root.exists() {
        match pin.kind {
            api::ChatHarnessKind::Tasci => {
                unreachable!("Tasci archive publication is rejected before staging an installation")
            }
            api::ChatHarnessKind::Codex => {
                fs::rename(staging.join(&pin.archive_prefix), &version_root)
                    .whatever("failed to publish Codex installation")?;
            }
            api::ChatHarnessKind::ClaudeCode => {
                let published = staging.join("published");
                prepare_directory(&published.join("bin"), 0o755)
                    .whatever("failed to prepare Claude Code installation")?;
                fs::rename(
                    staging.join(&pin.archive_prefix),
                    published.join("bin/claude"),
                )
                .whatever("failed to publish Claude Code installation")?;
                fs::set_permissions(
                    published.join("bin/claude"),
                    fs::Permissions::from_mode(0o555),
                )
                .whatever("failed to mark Claude Code executable")?;
                fs::rename(published, &version_root)
                    .whatever("failed to publish Claude Code installation directory")?;
            }
        }
    }
    let next = harness_root.join("current.next");
    remove_file_if_present(&next).whatever("failed to clear harness symlink staging path")?;
    std::os::unix::fs::symlink(pin.version, &next)
        .whatever("failed to create current harness symlink")?;
    fs::rename(next, harness_root.join("current"))
        .whatever("failed to publish current harness symlink")?;
    Ok(())
}

fn harness_pin(kind: &api::ChatHarnessKind) -> Result<HarnessPin, Report<HarnessManagerError>> {
    let architecture = std::env::consts::ARCH;
    match (kind, architecture) {
        (api::ChatHarnessKind::Tasci, _) => Err(Report::new(HarnessManagerError::InvalidRequest(
            "Tasci is bundled and has no downloadable harness pin".to_owned(),
        ))),
        (api::ChatHarnessKind::Codex, "x86_64") => Ok(HarnessPin {
            kind: kind.clone(),
            version: CODEX_VERSION,
            url: format!(
                "https://registry.npmjs.org/@openai/codex/-/codex-{CODEX_VERSION}-linux-x64.tgz"
            ),
            integrity: "2jxrmV6+/7eBNdg5uhhmOEPFu2o28eYY/ClLzWhSBHH8uo3f2KA1z9JQcVtwlbToW03nEPlEzYNYfCF1UBqsVQ==",
            archive_prefix: "package/vendor/x86_64-unknown-linux-musl".to_owned(),
        }),
        (api::ChatHarnessKind::Codex, "aarch64") => Ok(HarnessPin {
            kind: kind.clone(),
            version: CODEX_VERSION,
            url: format!(
                "https://registry.npmjs.org/@openai/codex/-/codex-{CODEX_VERSION}-linux-arm64.tgz"
            ),
            integrity: "OlKx65579OwIzech9Tt3OUH9+hFZfFrCBP1hL2MudnMIoNr1+cFZjB5YIj5MWMRoBD+K5W3wdBIpQSH855b5Sg==",
            archive_prefix: "package/vendor/aarch64-unknown-linux-musl".to_owned(),
        }),
        (api::ChatHarnessKind::ClaudeCode, "x86_64") => Ok(HarnessPin {
            kind: kind.clone(),
            version: CLAUDE_CODE_VERSION,
            url: format!(
                "https://registry.npmjs.org/@anthropic-ai/claude-code-linux-x64/-/claude-code-linux-x64-{CLAUDE_CODE_VERSION}.tgz"
            ),
            integrity: "zwDeTitQD3v0/GxX3hl1PTY54j7iF3/hA9QGFM95yind9hBkvLyv6aU9WdH7089s3AICfI6hZ74AFXtBRp6bzQ==",
            archive_prefix: "package/claude".to_owned(),
        }),
        (api::ChatHarnessKind::ClaudeCode, "aarch64") => Ok(HarnessPin {
            kind: kind.clone(),
            version: CLAUDE_CODE_VERSION,
            url: format!(
                "https://registry.npmjs.org/@anthropic-ai/claude-code-linux-arm64/-/claude-code-linux-arm64-{CLAUDE_CODE_VERSION}.tgz"
            ),
            integrity: "wyfnBQBkZpYKXQ2+SGqtJRqzGfm06zN94JoetsqJa8CYvCZvkzApfBRgqzsLALGBtLK7Xf13HdED+SPd1R3T9w==",
            archive_prefix: "package/claude".to_owned(),
        }),
        (_, architecture) => Err(Report::new(HarnessManagerError::InvalidRequest(format!(
            "chat harnesses do not support architecture {architecture}"
        )))),
    }
}

fn initial_harness(
    kind: api::ChatHarnessKind,
    display_name: &str,
    pinned_version: &str,
    installed: bool,
    credentials: bool,
) -> api::ChatHarness {
    api::ChatHarness {
        capabilities: capabilities(&kind),
        kind,
        display_name: display_name.into(),
        pinned_version: pinned_version.into(),
        installation: if installed {
            api::ChatHarnessInstallationState::Installed(api::ChatHarnessInstallation {
                installed_at: Timestamp::now(),
            })
        } else {
            api::ChatHarnessInstallationState::NotInstalled
        },
        credentials: if credentials {
            api::ChatHarnessCredentialState::Present
        } else {
            api::ChatHarnessCredentialState::Missing
        },
        validating_credentials: false,
        login: api::ChatHarnessLoginState::Idle,
        models: ArcVec::new(),
    }
}

fn initial_tasci_harness(installed: bool) -> api::ChatHarness {
    let mut harness = initial_harness(
        api::ChatHarnessKind::Tasci,
        "Tasci",
        TASCI_VERSION,
        installed,
        true,
    );
    harness.credentials =
        api::ChatHarnessCredentialState::Valid(api::ChatHarnessValidCredentials {
            method: "workspace-settings".into(),
            email: None,
            plan: None,
            checked_at: Timestamp::now(),
        });
    harness
}

fn capabilities(kind: &api::ChatHarnessKind) -> api::ChatHarnessCapabilities {
    match kind {
        api::ChatHarnessKind::Tasci => api::ChatHarnessCapabilities {
            resume_session: false,
            interrupt_turn: true,
            steer_turn: false,
            structured_user_input: false,
            compact_context: true,
            model_switching: api::ChatModelSwitching::InSession,
        },
        api::ChatHarnessKind::Codex => api::ChatHarnessCapabilities {
            resume_session: true,
            interrupt_turn: true,
            steer_turn: true,
            structured_user_input: true,
            compact_context: true,
            model_switching: api::ChatModelSwitching::InSession,
        },
        api::ChatHarnessKind::ClaudeCode => api::ChatHarnessCapabilities {
            resume_session: true,
            interrupt_turn: true,
            steer_turn: true,
            structured_user_input: true,
            compact_context: false,
            model_switching: api::ChatModelSwitching::InSession,
        },
    }
}

fn codex_executable(root: &Path) -> PathBuf {
    root.join("codex/current/bin/codex")
}

fn claude_executable(root: &Path) -> PathBuf {
    root.join("claude-code/current/bin/claude")
}

fn pod_codex_executable() -> PathBuf {
    PathBuf::from("/opt/tascarrel/harnesses/codex/current/bin/codex")
}

fn pod_claude_executable() -> PathBuf {
    PathBuf::from("/opt/tascarrel/harnesses/claude-code/current/bin/claude")
}

fn harness_directory_name(kind: &api::ChatHarnessKind) -> &'static str {
    match kind {
        api::ChatHarnessKind::Tasci => "tasci",
        api::ChatHarnessKind::Codex => "codex",
        api::ChatHarnessKind::ClaudeCode => "claude-code",
    }
}

fn prepare_directory(path: &Path, mode: u32) -> io::Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

fn prepare_credential_directory(
    path: &Path,
    owner_user_id: u32,
    owner_group_id: u32,
) -> Result<(), Report<HarnessManagerError>> {
    prepare_directory(path, 0o700).whatever("failed to prepare harness credential directory")?;
    nix::unistd::chown(
        path,
        Some(nix::unistd::Uid::from_raw(owner_user_id)),
        Some(nix::unistd::Gid::from_raw(owner_group_id)),
    )
    .whatever("failed to assign harness credential directory")
}

fn prepare_owned_credential_tree(
    root: &Path,
    owner_user_id: u32,
    owner_group_id: u32,
) -> Result<(), Report<HarnessManagerError>> {
    fn prepare(path: &Path, owner_user_id: u32, owner_group_id: u32) -> io::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            return Ok(());
        }
        if !file_type.is_dir() && !file_type.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "credential tree contains an unsupported file type",
            ));
        }
        nix::unistd::chown(
            path,
            Some(nix::unistd::Uid::from_raw(owner_user_id)),
            Some(nix::unistd::Gid::from_raw(owner_group_id)),
        )?;
        let mode = if file_type.is_dir() {
            0o700
        } else if metadata.permissions().mode() & 0o111 == 0 {
            0o600
        } else {
            0o700
        };
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
        if file_type.is_dir() {
            for entry in fs::read_dir(path)? {
                prepare(&entry?.path(), owner_user_id, owner_group_id)?;
            }
        }
        Ok(())
    }

    prepare(root, owner_user_id, owner_group_id)
        .whatever("failed to prepare owned harness credential state")
}

fn write_codex_config(
    root: &Path,
    owner_user_id: u32,
    owner_group_id: u32,
) -> Result<(), Report<HarnessManagerError>> {
    let path = root.join("config.toml");
    let mut file = open_credential_file(&path, true)
        .whatever("failed to open Codex credential configuration")?;
    file.set_len(0)
        .whatever("failed to reset Codex credential configuration")?;
    file.write_all(b"cli_auth_credentials_store = \"file\"\n")
        .whatever("failed to write Codex credential configuration")?;
    file.sync_all()
        .whatever("failed to flush Codex credential configuration")?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .whatever("failed to protect Codex credential configuration")?;
    nix::unistd::fchown(
        &file,
        Some(nix::unistd::Uid::from_raw(owner_user_id)),
        Some(nix::unistd::Gid::from_raw(owner_group_id)),
    )
    .whatever("failed to assign Codex credential configuration")
}

fn credentials_exist(root: &Path, candidates: &[&str]) -> io::Result<bool> {
    for name in candidates {
        let path = root.join(name);
        let file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
            .open(&path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "credential path is not a regular file",
            ));
        }
        if metadata.len() > 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

fn install_claude_token(
    root: &Path,
    token: &str,
    owner_user_id: u32,
    owner_group_id: u32,
) -> Result<(), Report<HarnessManagerError>> {
    let token = token.trim();
    if token.is_empty() || token.len() > MAX_SECRET_BYTES || token.chars().any(char::is_control) {
        return Err(Report::new(HarnessManagerError::InvalidRequest(
            "the Claude setup token is invalid".to_owned(),
        )));
    }
    let path = root.join("setup-token");
    let mut file =
        open_credential_file(&path, true).whatever("failed to open Claude setup token")?;
    file.set_len(0)
        .whatever("failed to reset Claude setup token")?;
    file.write_all(token.as_bytes())
        .whatever("failed to write Claude setup token")?;
    file.sync_all()
        .whatever("failed to flush Claude setup token")?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .whatever("failed to protect Claude setup token")?;
    nix::unistd::fchown(
        &file,
        Some(nix::unistd::Uid::from_raw(owner_user_id)),
        Some(nix::unistd::Gid::from_raw(owner_group_id)),
    )
    .whatever("failed to assign Claude setup token")
}

fn open_credential_file(path: &Path, create: bool) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(create)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW);
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "credential path is not a regular file",
        ));
    }
    Ok(file)
}

fn read_credential_file(path: &Path) -> io::Result<String> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "credential path is not a regular file",
        ));
    }
    if metadata.len() > MAX_SECRET_BYTES_U64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "credential file exceeds the size limit",
        ));
    }
    let mut value = String::new();
    file.take(MAX_SECRET_BYTES_U64 + 1)
        .read_to_string(&mut value)?;
    if value.len() > MAX_SECRET_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "credential file exceeds the size limit",
        ));
    }
    Ok(value)
}

fn valid_account_credentials(
    response: AccountReadResult,
) -> Result<api::ChatHarnessValidCredentials, Report<HarnessManagerError>> {
    let account = response.account.ok_or_else(|| {
        Report::new(HarnessManagerError::InvalidRequest(
            "Codex credentials were rejected by the provider".to_owned(),
        ))
    })?;
    Ok(api::ChatHarnessValidCredentials {
        method: account.method.into(),
        email: account.email.map(Into::into),
        plan: account.plan_type.map(Into::into),
        checked_at: Timestamp::now(),
    })
}

fn remove_file_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn harness_command_stopped(_result: HarnessCommandResult) {}

fn harness_binding_error(
    error: crate::services::chats::harness::protocol::HarnessError,
) -> HarnessBindingError {
    HarnessBindingError {
        code: format!("{:?}", error.kind),
        message: error.message,
    }
}

fn harness_manager_error(
    error: crate::services::chats::harness::protocol::HarnessError,
) -> Report<HarnessManagerError> {
    Report::new(HarnessManagerError::Internal(error.message))
}

#[allow(clippy::needless_pass_by_value)] // This signature is used directly with Result::map_err.
fn title_manager_error(report: Report<HarnessManagerError>) -> TitleGenerationError {
    TitleGenerationError {
        code: "harness_unavailable".to_owned(),
        message: report.to_string(),
    }
}

#[allow(clippy::needless_pass_by_value)] // This signature is used directly with Result::map_err.
fn manager_binding_error(error: Report<HarnessManagerError>) -> HarnessBindingError {
    HarnessBindingError {
        code: "harness_unavailable".to_owned(),
        message: error.to_string(),
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_environments_preserve_the_pod_user_home() {
        let codex_home = PathBuf::from("/opt/tascarrel/chat/harness-codex");
        let codex = HarnessEnvironment::codex(codex_home.clone())
            .variables()
            .unwrap();
        assert!(!codex.contains_key("HOME"));
        assert_eq!(
            codex.get("CODEX_HOME").map(String::as_str),
            codex_home.to_str()
        );
        assert_eq!(
            codex.get("CODEX_SQLITE_HOME").map(String::as_str),
            codex_home.to_str()
        );

        let claude_home = PathBuf::from("/opt/tascarrel/chat/harness-claude-code");
        let claude = HarnessEnvironment::claude(claude_home.clone())
            .variables()
            .unwrap();
        assert!(!claude.contains_key("HOME"));
        assert_eq!(
            claude.get("CLAUDE_CONFIG_DIR").map(String::as_str),
            claude_home.to_str()
        );
    }
}
