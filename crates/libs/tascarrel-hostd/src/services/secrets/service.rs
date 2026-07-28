//! Service coordination for configured secret providers.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_api::ArcStr;
use tascarrel_api::types::config;
use tascarrel_api::types::secrets as api;
use tascarrel_api::types::workspaces;
use tascarrel_api::types::workspaces::WorkspaceName;
use tascarrel_protocol::Framed;
use tascarrel_protocol::MAX_WORKSPACE_ENVIRONMENT_FAILURE_BYTES;
use tascarrel_protocol::MAX_WORKSPACE_ENVIRONMENT_FRAME_LEN;
use tascarrel_protocol::WorkspaceEnvironmentFailure;
use tascarrel_protocol::WorkspaceEnvironmentResponse;
use tascarrel_protocol::WorkspaceName as ValidatedWorkspaceName;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use tokio::time::timeout;
use tracing::debug;
use tracing::warn;

use super::interpolation::SecretTemplate;
use super::sops::SopsProvider;
use super::sops::SopsSnapshot;
use crate::services::config::ConfigService;
use crate::services::config::ConfigServiceError;
use crate::services::config::ConfigSubscription;
use crate::services::config::DEFAULT_MAX_CONFIG_BYTES;
use crate::services::config::load_config_file;
use crate::services::workspaces::WorkspaceEnvironmentRequest;
use crate::services::workspaces::WorkspaceEnvironmentRequests;

const MAX_INITIAL_SECRETS: usize = 16;
const MAX_INITIAL_SECRET_VALUE_BYTES: usize = 64 * 1024;
const MAX_SSH_PUBLIC_KEY_BYTES: u64 = 64 * 1024;

/// Resolves, inventories, and mutates workspace secret-provider instances.
#[derive(Clone)]
pub struct SecretsService {
    inner: Arc<SecretsServiceInner>,
}

impl SecretsService {
    /// Creates a service rooted at the host-owned workspace configuration
    /// directory.
    ///
    /// # Errors
    ///
    /// Returns [`SecretsServiceError::InvalidConfiguration`] when a resource
    /// bound is zero or the workspace root is not an absolute real
    /// directory.
    pub fn new(config: SecretsServiceConfig) -> Result<Self, Report<SecretsServiceError>> {
        config.validate()?;
        Ok(Self {
            inner: Arc::new(SecretsServiceInner {
                workspaces_directory: config.workspaces_directory,
                sops_executable: config.sops_executable,
                default_ssh_directory: config.default_ssh_directory,
                command_timeout: config.command_timeout,
                max_document_bytes: config.max_document_bytes,
                max_environment_requests: config.max_environment_requests,
                mutation_locks: Mutex::new(HashMap::new()),
                provider_snapshots: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// Encrypts initial provider values inside an unpublished workspace tree.
    ///
    /// The default SSH public key is selected from `id_ed25519.pub` and then
    /// `id_rsa.pub`. A decrypting round trip verifies that the corresponding
    /// private key is usable by hostd before the caller publishes the tree.
    ///
    /// # Errors
    ///
    /// Returns an error when a secret or provider is invalid, no supported
    /// default SSH key is available, or SOPS cannot encrypt and decrypt the
    /// resulting document.
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(secret_count = initial_secrets.len())
    )]
    pub(crate) async fn initialize_workspace_secrets(
        &self,
        workspace_directory: &Path,
        initial_secrets: &[workspaces::WorkspaceCreationSecret],
    ) -> Result<(), Report<SecretsServiceError>> {
        if initial_secrets.is_empty() {
            return Ok(());
        }
        if initial_secrets.len() > MAX_INITIAL_SECRETS {
            return Err(SecretsServiceError::invalid_request(format!(
                "at most {MAX_INITIAL_SECRETS} initial secrets may be supplied"
            )));
        }
        let pending = prepare_initial_providers(
            workspace_directory,
            initial_secrets,
            &self.inner.sops_executable,
            self.inner.command_timeout,
            self.inner.max_document_bytes,
        )?;

        let ssh_directory = self
            .inner
            .default_ssh_directory
            .as_ref()
            .ok_or_else(|| {
                SecretsServiceError::unavailable(
                    "HOME is unavailable; hostd cannot locate a default SOPS SSH key",
                )
            })?
            .clone();
        let recipient =
            tokio::task::spawn_blocking(move || default_sops_ssh_recipient(&ssh_directory))
                .await
                .map_err(|error| {
                    SecretsServiceError::internal("SSH public-key discovery task failed")
                        .message(error.to_string())
                })??;
        let provider_files = pending.keys().cloned().collect::<Vec<_>>();
        let sops_configuration = render_sops_configuration(&provider_files, &recipient);
        let configuration_directory = workspace_directory.to_owned();
        tokio::task::spawn_blocking(move || {
            write_new_private_file(
                &configuration_directory.join(".sops.yaml"),
                sops_configuration.as_bytes(),
            )
        })
        .await
        .map_err(|error| {
            SecretsServiceError::internal("SOPS configuration write task failed")
                .message(error.to_string())
        })??;

        for pending_provider in pending.values() {
            pending_provider
                .provider
                .store(&pending_provider.values)
                .await?;
            let snapshot = pending_provider.provider.load().await.map_err(|report| {
                report.message(
                    "the default SSH private key could not decrypt the initial SOPS document",
                )
            })?;
            if snapshot.values != pending_provider.values {
                return Err(SecretsServiceError::internal(
                    "SOPS initial-secret verification returned different values",
                ));
            }
        }
        Ok(())
    }

    /// Reveals one named secret through the currently configured provider.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider or secret name is invalid, the
    /// workspace configuration is unavailable, or the provider cannot decrypt
    /// its document.
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(
            workspace = %input.workspace_name.as_str(),
            provider = %input.provider_name,
            secret = %input.secret_name,
        )
    )]
    pub async fn reveal(
        &self,
        input: api::RevealSecretAction,
        config_service: &ConfigService,
    ) -> Result<api::RevealSecretOutput, Report<SecretsServiceError>> {
        validate_provider_name(input.provider_name.as_ref())?;
        validate_secret_name(input.secret_name.as_ref())?;
        let workspace_config = read_workspace_config(config_service, &input.workspace_name).await?;
        let provider = self.provider(
            &input.workspace_name,
            &workspace_config,
            input.provider_name.as_ref(),
        )?;
        let snapshot = self
            .load_provider(
                &input.workspace_name,
                input.provider_name.as_ref(),
                &provider,
            )
            .await?;
        let value = snapshot
            .values
            .get(input.secret_name.as_ref())
            .ok_or_else(|| SecretsServiceError::invalid_request("secret does not exist"))?;
        Ok(api::RevealSecretOutput {
            value: value.clone().into(),
        })
    }

    /// Creates or replaces one named secret through the currently configured
    /// provider.
    ///
    /// # Errors
    ///
    /// Returns an error when the request is invalid or stale, the workspace
    /// configuration is unavailable, or the provider cannot persist the
    /// mutation.
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(
            workspace = %input.workspace_name.as_str(),
            provider = %input.provider_name,
            secret = %input.secret_name,
        )
    )]
    pub async fn set(
        &self,
        input: api::SetSecretAction,
        config_service: &ConfigService,
    ) -> Result<api::SetSecretOutput, Report<SecretsServiceError>> {
        validate_provider_name(input.provider_name.as_ref())?;
        validate_secret_name(input.secret_name.as_ref())?;
        let workspace_config = read_workspace_config(config_service, &input.workspace_name).await?;
        let provider = self.provider(
            &input.workspace_name,
            &workspace_config,
            input.provider_name.as_ref(),
        )?;
        let mutation_lock = self
            .mutation_lock(&input.workspace_name, input.provider_name.as_ref())
            .await;
        let _mutation = mutation_lock.lock().await;
        let mut snapshot = self
            .load_provider(
                &input.workspace_name,
                input.provider_name.as_ref(),
                &provider,
            )
            .await?;
        require_revision(
            snapshot.revision.as_deref(),
            input.expected_revision.as_deref(),
        )?;
        snapshot
            .values
            .insert(input.secret_name.to_string(), input.value.to_string());
        let revision = provider.store(&snapshot.values).await?;
        self.cache_provider(
            &input.workspace_name,
            input.provider_name.as_ref(),
            &provider,
            SopsSnapshot {
                revision: Some(revision.clone()),
                values: snapshot.values,
            },
        )
        .await;
        Ok(api::SetSecretOutput {
            revision: revision.into(),
        })
    }

    /// Deletes one named secret through the currently configured provider.
    ///
    /// # Errors
    ///
    /// Returns an error when the request is invalid or stale, the workspace
    /// configuration is unavailable, or the provider cannot persist the
    /// mutation.
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(
            workspace = %input.workspace_name.as_str(),
            provider = %input.provider_name,
            secret = %input.secret_name,
        )
    )]
    pub async fn delete(
        &self,
        input: api::DeleteSecretAction,
        config_service: &ConfigService,
    ) -> Result<api::DeleteSecretOutput, Report<SecretsServiceError>> {
        validate_provider_name(input.provider_name.as_ref())?;
        validate_secret_name(input.secret_name.as_ref())?;
        let workspace_config = read_workspace_config(config_service, &input.workspace_name).await?;
        let provider = self.provider(
            &input.workspace_name,
            &workspace_config,
            input.provider_name.as_ref(),
        )?;
        let mutation_lock = self
            .mutation_lock(&input.workspace_name, input.provider_name.as_ref())
            .await;
        let _mutation = mutation_lock.lock().await;
        let mut snapshot = self
            .load_provider(
                &input.workspace_name,
                input.provider_name.as_ref(),
                &provider,
            )
            .await?;
        require_revision(
            snapshot.revision.as_deref(),
            input.expected_revision.as_deref(),
        )?;
        if snapshot.values.remove(input.secret_name.as_ref()).is_none() {
            return Err(SecretsServiceError::invalid_request(
                "secret does not exist",
            ));
        }
        let revision = provider.store(&snapshot.values).await?;
        self.cache_provider(
            &input.workspace_name,
            input.provider_name.as_ref(),
            &provider,
            SopsSnapshot {
                revision: Some(revision.clone()),
                values: snapshot.values,
            },
        )
        .await;
        Ok(api::DeleteSecretOutput {
            revision: revision.into(),
        })
    }

    /// Opens a metadata subscription driven by workspace configuration-tree
    /// changes.
    ///
    /// # Errors
    ///
    /// Returns an error when configuration observation cannot be started.
    pub async fn subscribe(
        &self,
        input: api::SecretsChangedSubscription,
        config_service: &ConfigService,
    ) -> Result<SecretsSubscription, Report<SecretsServiceError>> {
        let workspace_name = input.workspace_name;
        let subscription = config_service
            .subscribe(config::ConfigChangedSubscription {
                workspace_name: workspace_name.clone(),
            })
            .await
            .map_err(config_service_error)?;
        Ok(SecretsSubscription {
            service: self.clone(),
            workspace_name,
            config: subscription,
            previous: None,
        })
    }

    /// Resolves all secret references in the workspace's `[env]` values.
    ///
    /// # Errors
    ///
    /// Returns an error when interpolation syntax or a reference is invalid,
    /// or a configured provider cannot supply its document.
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(workspace = %workspace_name.as_str())
    )]
    pub async fn resolve_environment(
        &self,
        workspace_name: &WorkspaceName,
        workspace_config: &config::WorkspaceConfig,
    ) -> Result<HashMap<ArcStr, ArcStr>, Report<SecretsServiceError>> {
        let mut templates = Vec::new();
        for (name, value) in workspace_config.env.as_ref().into_iter().flatten() {
            templates.push((name.clone(), SecretTemplate::parse(value.as_ref())?));
        }
        let mut provider_values: HashMap<String, BTreeMap<String, String>> = HashMap::new();
        for (_, template) in &templates {
            for reference in template.references() {
                if !provider_values.contains_key(reference.provider()) {
                    let provider =
                        self.provider(workspace_name, workspace_config, reference.provider())?;
                    provider_values.insert(
                        reference.provider().to_owned(),
                        self.load_provider(workspace_name, reference.provider(), &provider)
                            .await?
                            .values,
                    );
                }
            }
        }
        let mut environment = HashMap::new();
        for (name, template) in templates {
            let rendered = template
                .render(|reference| {
                    provider_values
                        .get(reference.provider())?
                        .get(reference.secret())
                        .map(String::as_str)
                })
                .ok_or_else(|| {
                    SecretsServiceError::invalid_request(
                        "environment references a secret that does not exist",
                    )
                })?;
            if rendered.contains('\0') {
                return Err(SecretsServiceError::invalid_request(
                    "resolved environment value contains a NUL byte",
                ));
            }
            environment.insert(name, rendered.into());
        }
        Ok(environment)
    }

    /// Serves authenticated workspace requests for host-resolved startup
    /// environments.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) async fn serve_workspace_requests(
        &self,
        mut requests: WorkspaceEnvironmentRequests,
        config_service: ConfigService,
    ) {
        let mut transports = JoinSet::new();
        loop {
            tokio::select! {
                request = requests.recv(), if transports.len() < self.inner.max_environment_requests => {
                    let Some(request) = request else {
                        return;
                    };
                    let service = self.clone();
                    let config_service = config_service.clone();
                    transports.spawn(async move {
                        service
                            .serve_environment_channel(request, &config_service)
                            .await
                    });
                }
                Some(result) = transports.join_next(), if !transports.is_empty() => {
                    match result {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => debug!(%error, "guest environment transport closed"),
                        Err(error) => warn!(%error, "guest environment transport task failed"),
                    }
                }
            }
        }
    }

    /// Resolves one provider-qualified reference against a workspace
    /// configuration snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider is not configured or cannot supply
    /// the referenced secret.
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(
            workspace = %workspace_name.as_str(),
            provider = %reference.provider(),
            secret = %reference.secret(),
        )
    )]
    pub(crate) async fn resolve_reference(
        &self,
        workspace_name: &WorkspaceName,
        secrets_config: Option<&config::WorkspaceSecretsConfig>,
        reference: &SecretReference,
    ) -> Result<String, Report<SecretsServiceError>> {
        let provider =
            self.provider_in_config(workspace_name, secrets_config, reference.provider())?;
        self.load_provider(workspace_name, reference.provider(), &provider)
            .await?
            .values
            .remove(reference.secret())
            .ok_or_else(|| SecretsServiceError::invalid_request("secret does not exist"))
    }

    /// Builds secret-free metadata for every configured provider.
    async fn metadata(
        &self,
        workspace_name: &WorkspaceName,
        workspace_config: Option<&config::WorkspaceConfig>,
    ) -> api::SecretsChangedEvent {
        let mut providers = Vec::new();
        let configured = workspace_config
            .and_then(|config| config.secrets.as_ref())
            .and_then(|secrets| secrets.providers.as_ref());
        let mut names = configured
            .map(|providers| providers.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        names.sort_unstable();
        for name in names {
            let provider_config =
                &configured.expect("provider names require provider config")[&name];
            let kind = provider_kind(provider_config);
            let provider =
                self.provider_from_config(workspace_name, name.as_ref(), provider_config);
            let (revision, secrets, error) = match provider {
                Ok(provider) => match self
                    .load_provider(workspace_name, name.as_ref(), &provider)
                    .await
                {
                    Ok(snapshot) => (
                        snapshot.revision.map(Into::into),
                        snapshot
                            .values
                            .into_keys()
                            .map(|name| api::SecretMetadata { name: name.into() })
                            .collect::<Vec<_>>()
                            .into(),
                        None,
                    ),
                    Err(report) => (
                        None,
                        Vec::new().into(),
                        Some(api::SecretProviderError {
                            message: report.to_string().into(),
                        }),
                    ),
                },
                Err(report) => (
                    None,
                    Vec::new().into(),
                    Some(api::SecretProviderError {
                        message: report.to_string().into(),
                    }),
                ),
            };
            providers.push(api::SecretProviderMetadata {
                name,
                kind: kind.into(),
                capabilities: api::SecretProviderCapabilities {
                    reveal: true,
                    set: true,
                    delete: true,
                },
                revision,
                secrets,
                error,
            });
        }
        api::SecretsChangedEvent {
            providers: providers.into(),
        }
    }

    /// Resolves and sends one authenticated guest's startup environment.
    async fn serve_environment_channel(
        &self,
        request: WorkspaceEnvironmentRequest,
        config_service: &ConfigService,
    ) -> Result<(), Report<SecretsServiceError>> {
        let close_timeout = request.close_timeout;
        let result = match config_service.read(&request.workspace).await {
            Ok(event) => match event.config {
                Some(workspace_config) => self
                    .resolve_environment(&request.workspace, &workspace_config)
                    .await
                    .map(|environment| {
                        environment
                            .into_iter()
                            .map(|(name, value)| (name.to_string(), value.to_string()))
                            .collect::<BTreeMap<_, _>>()
                    }),
                None => Err(SecretsServiceError::unavailable(
                    "workspace config.toml is not currently valid",
                )),
            },
            Err(report) => Err(config_service_error(report)),
        };
        let response = WorkspaceEnvironmentResponse {
            result: result.map_err(|report| WorkspaceEnvironmentFailure {
                message: bounded_failure(&report.to_string()),
            }),
        };
        response.validate().map_err(|report| {
            report
                .escalate(SecretsServiceError::InvalidRequest)
                .message("resolved workspace environment violates protocol bounds")
        })?;
        let mut framed =
            Framed::with_max_frame_len(request.channel, MAX_WORKSPACE_ENVIRONMENT_FRAME_LEN)
                .map_err(|error| {
                    SecretsServiceError::internal(
                        "failed to configure workspace environment transport",
                    )
                    .message(error.to_string())
                })?;
        framed.write(&response).await.map_err(|error| {
            SecretsServiceError::unavailable("failed to send resolved workspace environment")
                .message(error.to_string())
        })?;
        let mut channel = framed.into_inner();
        match timeout(close_timeout, channel.close()).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                debug!(%error, "guest environment transport close failed");
            }
            Err(_) => {
                debug!("guest environment transport close timed out");
            }
        }
        Ok(())
    }

    /// Binds one provider from a complete workspace configuration.
    fn provider(
        &self,
        workspace_name: &WorkspaceName,
        workspace_config: &config::WorkspaceConfig,
        provider_name: &str,
    ) -> Result<SopsProvider, Report<SecretsServiceError>> {
        validate_provider_name(provider_name)?;
        self.provider_in_config(
            workspace_name,
            workspace_config.secrets.as_ref(),
            provider_name,
        )
    }

    /// Finds and binds one provider from optional provider configuration.
    fn provider_in_config(
        &self,
        workspace_name: &WorkspaceName,
        secrets_config: Option<&config::WorkspaceSecretsConfig>,
        provider_name: &str,
    ) -> Result<SopsProvider, Report<SecretsServiceError>> {
        let provider_config = secrets_config
            .and_then(|secrets| secrets.providers.as_ref())
            .and_then(|providers| providers.get(provider_name))
            .ok_or_else(|| {
                SecretsServiceError::invalid_request("secret provider is not configured")
            })?;
        self.provider_from_config(workspace_name, provider_name, provider_config)
    }

    /// Constructs the concrete provider selected by one configuration entry.
    fn provider_from_config(
        &self,
        workspace_name: &WorkspaceName,
        provider_name: &str,
        provider_config: &config::WorkspaceSecretProviderConfig,
    ) -> Result<SopsProvider, Report<SecretsServiceError>> {
        validate_provider_name(provider_name)?;
        let workspace_directory =
            workspace_directory(&self.inner.workspaces_directory, workspace_name)?;
        match provider_config {
            config::WorkspaceSecretProviderConfig::Sops(sops) => SopsProvider::new(
                workspace_directory,
                sops.file.as_deref(),
                self.inner.sops_executable.clone(),
                self.inner.command_timeout,
                self.inner.max_document_bytes,
            ),
        }
    }

    /// Returns the workspace-provider lock that serializes local mutations.
    async fn mutation_lock(
        &self,
        workspace_name: &WorkspaceName,
        provider_name: &str,
    ) -> Arc<Mutex<()>> {
        let key = (workspace_name.clone(), provider_name.to_owned());
        self.inner
            .mutation_locks
            .lock()
            .await
            .entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Returns a cached snapshot when its encrypted source revision is current.
    async fn load_provider(
        &self,
        workspace_name: &WorkspaceName,
        provider_name: &str,
        provider: &SopsProvider,
    ) -> Result<SopsSnapshot, Report<SecretsServiceError>> {
        let source_revision = provider.source_revision().await?;
        let key = (
            workspace_name.clone(),
            provider_name.to_owned(),
            provider.cache_identity(),
        );
        if let Some(snapshot) = self
            .inner
            .provider_snapshots
            .lock()
            .await
            .get(&key)
            .filter(|snapshot| snapshot.revision == source_revision)
            .cloned()
        {
            return Ok(snapshot);
        }
        let snapshot = provider.load().await?;
        self.inner
            .provider_snapshots
            .lock()
            .await
            .insert(key, snapshot.clone());
        Ok(snapshot)
    }

    /// Stores a snapshot after Tascarrel has persisted a mutation.
    async fn cache_provider(
        &self,
        workspace_name: &WorkspaceName,
        provider_name: &str,
        provider: &SopsProvider,
        snapshot: SopsSnapshot,
    ) {
        self.inner.provider_snapshots.lock().await.insert(
            (
                workspace_name.clone(),
                provider_name.to_owned(),
                provider.cache_identity(),
            ),
            snapshot,
        );
    }
}

struct PendingInitialProvider {
    provider: SopsProvider,
    values: BTreeMap<String, String>,
}

fn prepare_initial_providers(
    workspace_directory: &Path,
    initial_secrets: &[workspaces::WorkspaceCreationSecret],
    sops_executable: &Path,
    command_timeout: Duration,
    max_document_bytes: u64,
) -> Result<BTreeMap<String, PendingInitialProvider>, Report<SecretsServiceError>> {
    let workspace_config = load_config_file(
        &workspace_directory.join("config.toml"),
        DEFAULT_MAX_CONFIG_BYTES,
    )
    .map_err(|error| {
        SecretsServiceError::invalid_request(
            "workspace config.toml is not valid for initial secrets",
        )
        .message(error.to_string())
    })?;
    let configured_providers = workspace_config
        .secrets
        .as_ref()
        .and_then(|secrets| secrets.providers.as_ref())
        .ok_or_else(|| {
            SecretsServiceError::invalid_request(
                "initial secrets require configured SOPS providers",
            )
        })?;

    let mut pending = BTreeMap::<String, PendingInitialProvider>::new();
    for secret in initial_secrets {
        let provider_name = secret.provider_name.as_ref();
        let secret_name = secret.secret_name.as_ref();
        validate_initial_secret(provider_name, secret_name, secret.value.as_ref())?;
        let provider_config = configured_providers.get(provider_name).ok_or_else(|| {
            SecretsServiceError::invalid_request(format!(
                "initial secret provider {provider_name:?} is not configured"
            ))
        })?;
        let provider = match provider_config {
            config::WorkspaceSecretProviderConfig::Sops(sops) => SopsProvider::new(
                workspace_directory.to_owned(),
                sops.file.as_deref(),
                sops_executable.to_owned(),
                command_timeout,
                max_document_bytes,
            )?,
        };
        let pending_provider =
            pending
                .entry(provider.cache_identity())
                .or_insert_with(|| PendingInitialProvider {
                    provider,
                    values: BTreeMap::new(),
                });
        if pending_provider
            .values
            .insert(secret_name.to_owned(), secret.value.to_string())
            .is_some()
        {
            return Err(SecretsServiceError::invalid_request(format!(
                "initial secret {provider_name}.{secret_name} is duplicated"
            )));
        }
    }
    Ok(pending)
}

fn validate_initial_secret(
    provider_name: &str,
    secret_name: &str,
    value: &str,
) -> Result<(), Report<SecretsServiceError>> {
    validate_provider_name(provider_name)?;
    validate_secret_name(secret_name)?;
    if value.is_empty() {
        return Err(SecretsServiceError::invalid_request(
            "initial secret values must not be empty",
        ));
    }
    if value.len() > MAX_INITIAL_SECRET_VALUE_BYTES {
        return Err(SecretsServiceError::invalid_request(format!(
            "initial secret values must not exceed {MAX_INITIAL_SECRET_VALUE_BYTES} bytes"
        )));
    }
    Ok(())
}

fn default_sops_ssh_recipient(ssh_directory: &Path) -> Result<String, Report<SecretsServiceError>> {
    for key_name in ["id_ed25519", "id_rsa"] {
        let private_key = ssh_directory.join(key_name);
        let public_key = ssh_directory.join(format!("{key_name}.pub"));
        if !std::fs::metadata(&private_key).is_ok_and(|metadata| metadata.is_file()) {
            continue;
        }
        let Ok(metadata) = std::fs::metadata(&public_key) else {
            continue;
        };
        if !metadata.is_file() || metadata.len() > MAX_SSH_PUBLIC_KEY_BYTES {
            continue;
        }
        let Ok(contents) = std::fs::read_to_string(&public_key) else {
            continue;
        };
        if let Some(recipient) = parse_sops_ssh_recipient(&contents) {
            return Ok(recipient);
        }
    }
    Err(SecretsServiceError::unavailable(format!(
        "no usable default SOPS SSH key found in {}; expected matching id_ed25519(.pub) or id_rsa(.pub) files",
        ssh_directory.display()
    )))
}

fn parse_sops_ssh_recipient(contents: &str) -> Option<String> {
    for line in contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let mut fields = line.split_ascii_whitespace();
        let (Some(key_type), Some(encoded)) = (fields.next(), fields.next()) else {
            continue;
        };
        if !matches!(key_type, "ssh-ed25519" | "ssh-rsa") {
            continue;
        }
        let Ok(decoded) = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(encoded))
        else {
            continue;
        };
        let Some(encoded_key_type) = ssh_blob_first_field(&decoded) else {
            continue;
        };
        if encoded_key_type != key_type.as_bytes() {
            continue;
        }
        return Some(format!("{key_type} {encoded}"));
    }
    None
}

fn ssh_blob_first_field(blob: &[u8]) -> Option<&[u8]> {
    let length = u32::from_be_bytes(blob.get(..4)?.try_into().ok()?);
    let length = usize::try_from(length).ok()?;
    blob.get(4..4_usize.checked_add(length)?)
}

fn render_sops_configuration(provider_files: &[String], recipient: &str) -> String {
    let mut configuration = String::from("creation_rules:\n");
    for file in provider_files {
        configuration.push_str("  - path_regex: '");
        configuration.push('^');
        for character in file.chars() {
            if matches!(
                character,
                '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\'
            ) {
                configuration.push('\\');
            }
            configuration.push(character);
        }
        configuration.push('$');
        configuration.push_str("'\n    age: '");
        configuration.push_str(&recipient.replace('\'', "''"));
        configuration.push_str("'\n");
    }
    configuration
}

fn write_new_private_file(path: &Path, contents: &[u8]) -> Result<(), Report<SecretsServiceError>> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| {
            SecretsServiceError::unavailable("failed to create SOPS configuration")
                .message(error.to_string())
        })?;
    file.write_all(contents).map_err(|error| {
        SecretsServiceError::unavailable("failed to write SOPS configuration")
            .message(error.to_string())
    })?;
    file.sync_all().map_err(|error| {
        SecretsServiceError::unavailable("failed to synchronize SOPS configuration")
            .message(error.to_string())
    })?;
    let parent = path.parent().ok_or_else(|| {
        SecretsServiceError::internal("SOPS configuration path has no parent directory")
    })?;
    std::fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            SecretsServiceError::unavailable("failed to synchronize workspace configuration")
                .message(error.to_string())
        })
}

/// Filesystem and subprocess bounds for [`SecretsService`].
#[derive(Clone, Debug)]
pub struct SecretsServiceConfig {
    /// Directory containing one configuration directory per workspace.
    pub workspaces_directory: PathBuf,
    /// Host `sops` executable or executable name.
    pub sops_executable: PathBuf,
    /// User SSH directory searched for the default SOPS recipient.
    ///
    /// This defaults to `$HOME/.ssh` when `HOME` is an absolute path. The
    /// directory is required only when creating a workspace with initial
    /// secrets.
    pub default_ssh_directory: Option<PathBuf>,
    /// Maximum duration of one SOPS command.
    pub command_timeout: Duration,
    /// Maximum encrypted or decrypted document size.
    pub max_document_bytes: u64,
    /// Maximum guest environment requests resolved concurrently.
    pub max_environment_requests: usize,
}

impl SecretsServiceConfig {
    /// Default maximum number of guest environment requests resolved
    /// concurrently.
    pub const DEFAULT_MAX_ENVIRONMENT_REQUESTS: usize = 64;

    /// Creates service configuration with the `sops` executable, a 30-second
    /// timeout, the workspace configuration size limit, and bounded guest
    /// environment resolution concurrency.
    #[must_use]
    pub fn new(
        workspaces_directory: impl Into<PathBuf>,
        sops_executable: impl Into<PathBuf>,
    ) -> Self {
        Self {
            workspaces_directory: workspaces_directory.into(),
            sops_executable: sops_executable.into(),
            default_ssh_directory: std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|home| home.is_absolute())
                .map(|home| home.join(".ssh")),
            command_timeout: Duration::from_secs(30),
            max_document_bytes: tascarrel_api::MAX_WORKSPACE_CONFIG_BYTES,
            max_environment_requests: Self::DEFAULT_MAX_ENVIRONMENT_REQUESTS,
        }
    }

    /// Validates service paths and resource bounds.
    fn validate(&self) -> Result<(), Report<SecretsServiceError>> {
        if !self.workspaces_directory.is_absolute() {
            return Err(SecretsServiceError::invalid_configuration(
                "workspace configuration directory must be absolute",
            ));
        }
        let metadata = std::fs::symlink_metadata(&self.workspaces_directory).map_err(|error| {
            SecretsServiceError::invalid_configuration(
                "failed to inspect workspace configuration directory",
            )
            .message(error.to_string())
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(SecretsServiceError::invalid_configuration(
                "workspace configuration root must be a real directory",
            ));
        }
        if self.sops_executable.as_os_str().is_empty()
            || self.command_timeout.is_zero()
            || self.max_document_bytes == 0
            || self.max_environment_requests == 0
        {
            return Err(SecretsServiceError::invalid_configuration(
                "SOPS executable must be non-empty and secret service resource bounds must be non-zero",
            ));
        }
        if !self.sops_executable.is_absolute() && self.sops_executable.components().count() != 1 {
            return Err(SecretsServiceError::invalid_configuration(
                "SOPS executable must be absolute or a bare program name",
            ));
        }
        if self
            .default_ssh_directory
            .as_ref()
            .is_some_and(|directory| !directory.is_absolute())
        {
            return Err(SecretsServiceError::invalid_configuration(
                "default SSH directory must be absolute",
            ));
        }
        Ok(())
    }
}

/// Metadata subscription backed by configuration-tree observation.
pub struct SecretsSubscription {
    service: SecretsService,
    workspace_name: WorkspaceName,
    config: ConfigSubscription,
    previous: Option<api::SecretsChangedEvent>,
}

impl SecretsSubscription {
    /// Receives current metadata and then metadata after each
    /// configuration-tree change.
    pub async fn recv(&mut self) -> Option<api::SecretsChangedEvent> {
        loop {
            let config = self.config.recv().await?;
            let event = self
                .service
                .metadata(&self.workspace_name, config.config.as_ref())
                .await;
            if self.previous.as_ref() == Some(&event) {
                continue;
            }
            self.previous = Some(event.clone());
            return Some(event);
        }
    }
}

/// Parsed provider-qualified secret reference.
#[derive(Clone, Eq, Hash, PartialEq)]
pub(crate) struct SecretReference {
    provider: String,
    secret: String,
}

impl SecretReference {
    /// Parses `<provider>.<secret>` and validates both namespace components.
    ///
    /// # Errors
    ///
    /// Returns an invalid-request report when the reference is not exactly two
    /// valid namespace components.
    pub(crate) fn parse(value: &str) -> Result<Self, Report<SecretsServiceError>> {
        let Some((provider, secret)) = value.split_once('.') else {
            return Err(SecretsServiceError::invalid_request(
                "secret reference must have the form <provider>.<secret>",
            ));
        };
        if secret.contains('.') {
            return Err(SecretsServiceError::invalid_request(
                "secret reference must contain exactly one namespace separator",
            ));
        }
        validate_provider_name(provider)?;
        validate_secret_name(secret)?;
        Ok(Self {
            provider: provider.to_owned(),
            secret: secret.to_owned(),
        })
    }

    /// Returns the provider-name component.
    pub(crate) fn provider(&self) -> &str {
        &self.provider
    }

    /// Returns the secret-name component.
    pub(crate) fn secret(&self) -> &str {
        &self.secret
    }
}

/// Caller-relevant secret service failure categories.
#[derive(Debug, Error)]
pub enum SecretsServiceError {
    /// Service construction input is invalid.
    #[error("secret service configuration is invalid")]
    InvalidConfiguration,
    /// A provider, reference, mutation, or optimistic revision is invalid.
    #[error("secret service request is invalid")]
    InvalidRequest,
    /// Provider files or executables are currently unavailable.
    #[error("secret provider is unavailable")]
    Unavailable,
    /// Unexpected serialization or task coordination failed.
    #[error("secret service operation failed")]
    Internal,
}

impl SecretsServiceError {
    /// Reports an invalid service construction input.
    pub(crate) fn invalid_configuration(message: impl Into<String>) -> Report<Self> {
        Self::InvalidConfiguration.report().message(message.into())
    }

    /// Reports an invalid provider operation or reference.
    pub(crate) fn invalid_request(message: impl Into<String>) -> Report<Self> {
        Self::InvalidRequest.report().message(message.into())
    }

    /// Reports a provider resource that cannot currently be used.
    pub(crate) fn unavailable(message: impl Into<String>) -> Report<Self> {
        Self::Unavailable.report().message(message.into())
    }

    /// Reports an unexpected service coordination failure.
    pub(crate) fn internal(message: impl Into<String>) -> Report<Self> {
        Self::Internal.report().message(message.into())
    }
}

/// Validates a secret name shared by provider documents and references.
///
/// # Errors
///
/// Returns an invalid-request report for names outside the portable secret
/// namespace or for the provider-reserved `sops` key.
pub(crate) fn validate_secret_name(name: &str) -> Result<(), Report<SecretsServiceError>> {
    let mut characters = name.chars();
    if name == "sops"
        || !characters
            .next()
            .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        || !characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
    {
        return Err(SecretsServiceError::invalid_request(
            "secret name must start with an ASCII letter or underscore and contain only letters, digits, and underscores; sops is reserved",
        ));
    }
    Ok(())
}

/// Validates the provider portion of a secret namespace.
fn validate_provider_name(name: &str) -> Result<(), Report<SecretsServiceError>> {
    let mut characters = name.chars();
    if !characters
        .next()
        .is_some_and(|character| character.is_ascii_alphabetic())
        || !characters.all(|character| {
            character == '_' || character == '-' || character.is_ascii_alphanumeric()
        })
    {
        return Err(SecretsServiceError::invalid_request(
            "provider name must start with an ASCII letter and contain only letters, digits, underscores, and hyphens",
        ));
    }
    Ok(())
}

/// Rejects a mutation based on an obsolete encrypted-source revision.
fn require_revision(
    actual: Option<&str>,
    expected: Option<&str>,
) -> Result<(), Report<SecretsServiceError>> {
    if expected.is_some_and(|expected| actual != Some(expected)) {
        return Err(SecretsServiceError::invalid_request(
            "secret provider changed; reload it and try again",
        ));
    }
    Ok(())
}

/// Reads the latest valid workspace configuration for a provider operation.
async fn read_workspace_config(
    config_service: &ConfigService,
    workspace_name: &WorkspaceName,
) -> Result<config::WorkspaceConfig, Report<SecretsServiceError>> {
    let event = config_service
        .read(workspace_name)
        .await
        .map_err(config_service_error)?;
    event.config.ok_or_else(|| {
        SecretsServiceError::unavailable("workspace config.toml is not currently valid")
    })
}

/// Maps configuration failures into caller-relevant secret service categories.
fn config_service_error(report: Report<ConfigServiceError>) -> Report<SecretsServiceError> {
    match report.error() {
        ConfigServiceError::InvalidRequest => report.escalate(SecretsServiceError::InvalidRequest),
        ConfigServiceError::Unavailable => report.escalate(SecretsServiceError::Unavailable),
        ConfigServiceError::InvalidConfiguration
        | ConfigServiceError::RuntimeUnavailable
        | ConfigServiceError::Internal => report.escalate(SecretsServiceError::Internal),
    }
}

/// Resolves a validated workspace name below the configured service root.
fn workspace_directory(
    root: &std::path::Path,
    workspace_name: &WorkspaceName,
) -> Result<PathBuf, Report<SecretsServiceError>> {
    let name: ArcStr = workspace_name.clone().into();
    let validated = ValidatedWorkspaceName::new(name.to_string()).map_err(|error| {
        SecretsServiceError::invalid_request("workspace name is invalid").message(error.to_string())
    })?;
    Ok(root.join(validated.as_str()))
}

/// Returns the stable implementation identifier exposed through metadata.
fn provider_kind(provider: &config::WorkspaceSecretProviderConfig) -> &'static str {
    match provider {
        config::WorkspaceSecretProviderConfig::Sops(_) => "sops",
    }
}

/// Truncates guest-facing diagnostics at a UTF-8 boundary.
fn bounded_failure(message: &str) -> String {
    if message.len() <= MAX_WORKSPACE_ENVIRONMENT_FAILURE_BYTES {
        return message.to_owned();
    }
    let mut end = MAX_WORKSPACE_ENVIRONMENT_FAILURE_BYTES;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message[..end].to_owned()
}

/// Shared service paths, limits, locks, and decrypted snapshots.
struct SecretsServiceInner {
    workspaces_directory: PathBuf,
    sops_executable: PathBuf,
    default_ssh_directory: Option<PathBuf>,
    command_timeout: Duration,
    max_document_bytes: u64,
    max_environment_requests: usize,
    mutation_locks: Mutex<ProviderMutationLocks>,
    provider_snapshots: Mutex<HashMap<(WorkspaceName, String, String), SopsSnapshot>>,
}

/// Mutation locks keyed by workspace and provider name.
type ProviderMutationLocks = HashMap<(WorkspaceName, String), Arc<Mutex<()>>>;

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;

    use tascarrel_mux::Config as MuxConfig;
    use tascarrel_mux::Role as MuxRole;
    use tascarrel_mux::connect as connect_mux;
    use tokio::io::AsyncWriteExt as _;

    use super::*;
    use crate::services::config::ConfigServiceConfig;

    /// Initial creation encrypts provider values and emits only the normalized
    /// public SSH recipient into the SOPS policy.
    #[tokio::test]
    async fn initial_workspace_secrets_are_encrypted_with_the_default_ssh_key() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join(".workspace-demo-staging");
        fs::create_dir(&workspace).unwrap();
        fs::write(
            workspace.join("config.toml"),
            "[secrets.providers.services]\nkind = \"sops\"\nfile = \"secrets.json\"\n",
        )
        .unwrap();
        let ssh_directory = root.path().join("ssh");
        fs::create_dir(&ssh_directory).unwrap();
        fs::write(ssh_directory.join("id_ed25519"), "test-private-key").unwrap();
        let mut public_key_blob = Vec::from(11_u32.to_be_bytes());
        public_key_blob.extend_from_slice(b"ssh-ed25519");
        public_key_blob.extend_from_slice(&32_u32.to_be_bytes());
        public_key_blob.extend_from_slice(&[7_u8; 32]);
        let encoded_public_key = base64::engine::general_purpose::STANDARD.encode(public_key_blob);
        fs::write(
            ssh_directory.join("id_ed25519.pub"),
            format!("ssh-ed25519 {encoded_public_key} user@example\n"),
        )
        .unwrap();
        let executable = root.path().join("fake-sops");
        fs::write(
            &executable,
            "#!/bin/sh\nset -eu\ntest -f .sops.yaml\noperation=$1\nshift\n\
             while [ \"$#\" -gt 1 ]; do shift; done\ncase \"$operation\" in\n\
             encrypt) base64 ;;\ndecrypt) base64 -d ;;\n*) exit 2 ;;\nesac\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let mut service_config = SecretsServiceConfig::new(root.path(), &executable);
        service_config.default_ssh_directory = Some(ssh_directory);
        let service = SecretsService::new(service_config).unwrap();
        let values = vec![
            workspaces::WorkspaceCreationSecret {
                provider_name: "services".into(),
                secret_name: "GITHUB_TOKEN".into(),
                value: "github_pat_read_only_example".into(),
            },
            workspaces::WorkspaceCreationSecret {
                provider_name: "services".into(),
                secret_name: "GITLAB_TOKEN".into(),
                value: "glpat-read-only-example".into(),
            },
        ];

        service
            .initialize_workspace_secrets(&workspace, &values)
            .await
            .unwrap();

        let encrypted = fs::read(workspace.join("secrets.json")).unwrap();
        assert!(
            !encrypted
                .windows("github_pat_read_only_example".len())
                .any(|bytes| bytes == b"github_pat_read_only_example")
        );
        assert_eq!(
            fs::read_to_string(workspace.join(".sops.yaml")).unwrap(),
            format!(
                "creation_rules:\n  - path_regex: '^secrets\\.json$'\n    age: 'ssh-ed25519 {encoded_public_key}'\n"
            ),
        );
        assert_eq!(
            fs::metadata(workspace.join(".sops.yaml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0,
        );
        let provider = SopsProvider::new(
            workspace,
            Some("secrets.json"),
            executable,
            Duration::from_secs(5),
            1024 * 1024,
        )
        .unwrap();
        assert_eq!(
            provider.load().await.unwrap().values,
            BTreeMap::from([
                (
                    "GITHUB_TOKEN".to_owned(),
                    "github_pat_read_only_example".to_owned(),
                ),
                (
                    "GITLAB_TOKEN".to_owned(),
                    "glpat-read-only-example".to_owned(),
                ),
            ]),
        );
    }

    /// A delayed guest still receives the complete resolved environment before
    /// the host releases its one-shot mux channel.
    #[tokio::test]
    async fn environment_response_survives_a_delayed_guest_read() {
        let root = tempfile::tempdir().unwrap();
        let workspace_name = WorkspaceName::new("transport-test");
        let workspace = root.path().join(workspace_name.as_str());
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(workspace.join("image")).unwrap();
        fs::write(
            workspace.join("config.toml"),
            "[env]\nONMCU_API_KEY = \"${secrets.workspace.ONMCU_API_KEY}\"\n\
             [secrets.providers.workspace]\nkind = \"sops\"\n",
        )
        .unwrap();
        fs::write(workspace.join(".sops.yaml"), "creation_rules: []\n").unwrap();
        let executable = workspace.join("fake-sops");
        fs::write(
            &executable,
            "#!/bin/sh\nset -eu\ntest -f .sops.yaml\noperation=$1\nshift\n\
             while [ \"$#\" -gt 1 ]; do shift; done\ncase \"$operation\" in\n\
             encrypt) base64 ;;\ndecrypt) base64 -d ;;\n*) exit 2 ;;\nesac\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let provider = SopsProvider::new(
            workspace.clone(),
            None,
            executable.clone(),
            Duration::from_secs(5),
            1024 * 1024,
        )
        .unwrap();
        provider
            .store(&BTreeMap::from([(
                "ONMCU_API_KEY".to_owned(),
                "resolved-value".to_owned(),
            )]))
            .await
            .unwrap();

        let config_service = ConfigService::open(ConfigServiceConfig::new(root.path())).unwrap();
        let secrets_service =
            SecretsService::new(SecretsServiceConfig::new(root.path(), executable)).unwrap();
        let (guest_io, host_io) = tokio::io::duplex(1024 * 1024);
        let (guest_driver, guest_mux, _) =
            connect_mux(guest_io, MuxRole::Client, MuxConfig::default()).unwrap();
        let (host_driver, _, mut host_incoming) =
            connect_mux(host_io, MuxRole::Server, MuxConfig::default()).unwrap();
        let guest_driver = tokio::spawn(guest_driver.run());
        let host_driver = tokio::spawn(host_driver.run());
        let (guest_channel, host_channel) =
            tokio::join!(guest_mux.open("workspace-environment"), async {
                host_incoming
                    .recv()
                    .await
                    .expect("guest opens an environment channel")
                    .accept()
            },);
        let mut guest_channel = guest_channel.unwrap();
        let host_channel = host_channel.unwrap();
        let guest_response = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let mut framed =
                Framed::with_max_frame_len(guest_channel, MAX_WORKSPACE_ENVIRONMENT_FRAME_LEN)
                    .unwrap();
            let response = framed
                .read::<WorkspaceEnvironmentResponse>()
                .await
                .unwrap()
                .expect("host sends an environment response");
            guest_channel = framed.into_inner();
            guest_channel.shutdown().await.unwrap();
            response
        });

        secrets_service
            .serve_environment_channel(
                WorkspaceEnvironmentRequest {
                    workspace: workspace_name,
                    channel: host_channel,
                    close_timeout: Duration::from_secs(1),
                },
                &config_service,
            )
            .await
            .unwrap();
        let environment = guest_response.await.unwrap().result.unwrap();
        assert_eq!(environment["ONMCU_API_KEY"], "resolved-value");

        guest_driver.abort();
        host_driver.abort();
    }
}
