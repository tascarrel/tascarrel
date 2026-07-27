//! Concrete image generation lifecycle and resumable image inventory.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use async_trait::async_trait;
use jiff::Timestamp;
use reportify::ErrorExt as _;
use reportify::Report;
use reportify::ResultExt as _;
use serde::Deserialize as _;
use tascarrel_api::ArcVec;
use tascarrel_api::types::images as api;
use tascarrel_api::types::store as store_api;
use tascarrel_protocol::Health;
use tascarrel_protocol::Pod as NetworkPrincipal;
use tascarrel_protocol::PodId as NetworkPrincipalId;
use tascarrel_store::Store;
use thiserror::Error;
use tokio::sync::watch;
use tokio::task;
use tracing::warn;
use uuid::Uuid;

use super::ImageBuildOutcome;
use super::ImageBuilder;
use super::ImageBuilderConfig;
use super::input::ImageInputLimits;
use super::input::ImageInputSnapshot;
use super::input::snapshot;
use super::log::ImageLogWriter;
use super::log::LogBuffer;
use super::log::LogSubscription;
use super::state::ImageRecord;
use super::state::ImageStateError;
use super::state::ImageStateRepository;
use crate::Database;
use crate::GuestNetworkService;
use crate::NetworkManager;
use crate::PodNetwork;
use crate::repositories::RepositoryPreparation;
use crate::runtime::network::BUILD_NETWORK_NAMESPACE;
use crate::runtime::pod::BtrfsStore;
use crate::runtime::pod::ImageId as RuntimeImageId;
use crate::runtime::pod::StoreError;
use crate::services::pods::PodService;
use crate::services::processes::ProcessSupervisor;

/// Refreshes the host-published image definition before an image operation.
///
/// The dependency is supplied at operation time so the image service remains
/// independent of the host transport and external development mode can use a
/// no-op implementation.
#[async_trait]
pub trait ImageInputRefresh: Send + Sync {
    /// Publishes the latest image input and returns its immutable directory.
    ///
    /// External development mode returns `None` to use the service's
    /// configured static directory.
    async fn refresh_image_input(&self) -> Result<Option<PathBuf>, Report<ImageServiceError>>;
}

/// Owns workspace image input inspection, building, storage, and durable state.
///
/// The service receives the storage-owned Btrfs store and constructs its own
/// builder and network manager. Those concrete runtime resources remain
/// private to the service.
#[derive(Clone)]
pub struct ImageService {
    inner: Arc<ImageServiceInner>,
}

impl ImageService {
    /// Opens image storage, recovers its database inventory, and cleans
    /// interrupted build resources.
    ///
    /// # Errors
    ///
    /// Returns [`ImageServiceError::Internal`] when database recovery, storage,
    /// builder, or build network initialization fails.
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(store = %storage.root().display())
    )]
    pub async fn open(
        mut config: ImageServiceConfig,
        database: Database,
        storage: Arc<BtrfsStore>,
    ) -> Result<Self, Report<ImageServiceError>> {
        let temporary_root = config.builder.temporary_root.clone();
        blocking("clean stale image build directories", move || {
            super::cleanup_stale_image_build_directories(temporary_root).map(|_| ())
        })
        .await?;
        config.builder.nsenter = config.nsenter_program.clone();
        config.builder.network_namespace = Some(BUILD_NETWORK_NAMESPACE.to_owned());
        let input_limits = ImageInputLimits {
            entries: config.builder.limits.max_context_entries,
            bytes: config.builder.limits.max_context_bytes,
            depth: config.builder.limits.max_context_depth,
        };
        let builder = ImageBuilder::new(config.builder)
            .map_err(|error| internal(format!("configure image builder: {error}")))?;
        let network = NetworkManager::new(config.ip_program, config.nsenter_program)
            .map_err(|error| internal(format!("configure image build network: {error}")))?;
        let build_network = PodNetwork::for_build();
        network
            .remove_named(&build_network, BUILD_NETWORK_NAMESPACE)
            .await
            .map_err(|error| internal(format!("clean image build network: {error}")))?;

        let state = ImageStateRepository::new(database);
        let mut records = state.recover().await.map_err(state_error)?;
        let tracked_generations = records
            .iter()
            .filter_map(|record| record.generation.clone())
            .collect::<BTreeSet<_>>();
        let reconciled_storage = Arc::clone(&storage);
        let available_generations = blocking("reconcile stored image generations", move || {
            reconcile_storage(&reconciled_storage, &tracked_generations)
        })
        .await?;
        let mut availability = Vec::new();
        let mut images = BTreeMap::new();
        let mut initial_images = ArcVec::new();
        for mut record in records.drain(..) {
            if let Some(generation) = &record.generation {
                let available = available_generations.contains(generation);
                record.image.state = if available {
                    api::ImageState::Available
                } else {
                    api::ImageState::Orphaned
                };
                availability.push((record.image.id.clone(), available));
            }
            initial_images.push(record.image.clone());
            let log = LogBuffer::new(config.log_capacity, config.log_batch_capacity);
            log.close();
            images.insert(
                record.image.id.clone(),
                Arc::new(ManagedImage {
                    completion: completion_channel(&record),
                    record: Mutex::new(record),
                    log,
                }),
            );
        }
        state.reconcile(availability).await.map_err(state_error)?;
        let list = Store::new(
            api::ImageList {
                images: initial_images,
            },
            reduce_image_list,
            config.store_history_limit,
        );
        Ok(Self {
            inner: Arc::new(ImageServiceInner {
                builder: Arc::new(builder),
                storage,
                network,
                build_network,
                image_definition_directory: config.image_definition_directory,
                input_limits,
                log_capacity: config.log_capacity,
                log_batch_capacity: config.log_batch_capacity,
                max_log_line_bytes: config.max_log_line_bytes,
                setup_scripts: config.setup_scripts,
                state,
                images: Mutex::new(images),
                list,
                operation: tokio::sync::Mutex::new(()),
            }),
        })
    }

    /// Admits one build and returns after its generating state is observable.
    ///
    /// # Errors
    ///
    /// Returns [`ImageServiceError::InvalidRequest`] when another image is
    /// generating. Returns [`ImageServiceError::Internal`] when the current
    /// host-backed image definition cannot be fingerprinted safely.
    #[tracing::instrument(level = "debug", skip_all)]
    pub(crate) async fn build(
        &self,
        _input: api::BuildImageAction,
        pods: &PodService,
        processes: &ProcessSupervisor,
        network_service: Arc<GuestNetworkService>,
        input_refresh: &dyn ImageInputRefresh,
        repositories: Option<RepositoryPreparation>,
    ) -> Result<api::BuildImageOutput, Report<ImageServiceError>> {
        let input_directory = self
            .input_directory(input_refresh, repositories.as_ref())
            .await?;
        self.build_current_input(
            pods,
            processes,
            network_service,
            repositories,
            input_directory,
        )
        .await
    }

    async fn build_current_input(
        &self,
        pods: &PodService,
        processes: &ProcessSupervisor,
        network_service: Arc<GuestNetworkService>,
        repositories: Option<RepositoryPreparation>,
        input_directory: PathBuf,
    ) -> Result<api::BuildImageOutput, Report<ImageServiceError>> {
        if lock(&self.inner.images)
            .values()
            .any(|image| image.is_generating())
        {
            return Err(invalid_request("an image is already generating"));
        }
        let limits = self.inner.input_limits;
        let input = task::spawn_blocking(move || snapshot(&input_directory, limits))
            .await
            .map_err(|error| internal(format!("fingerprint image input task failed: {error}")))?
            .map_err(|error| {
                error.escalate(ImageServiceError::Internal(
                    "fingerprint image input failed".to_owned(),
                ))
            })?;
        let _operation = self.inner.operation.lock().await;
        if lock(&self.inner.images)
            .values()
            .any(|image| image.is_generating())
        {
            return Err(invalid_request("an image is already generating"));
        }
        let image_id = api::ImageId::generate();
        let image = api::Image {
            id: image_id.clone(),
            input: api_input(&input),
            state: api::ImageState::Generating,
            created_at: Timestamp::now(),
        };
        if lock(&self.inner.images).contains_key(&image_id) {
            return Err(internal("generated a duplicate image identifier"));
        }
        let record = ImageRecord {
            image: image.clone(),
            generation: None,
        };
        self.inner
            .state
            .create(record.clone())
            .await
            .map_err(state_error)?;
        let log = LogBuffer::new(self.inner.log_capacity, self.inner.log_batch_capacity);
        let writer = ImageLogWriter::new(log.clone(), self.inner.max_log_line_bytes);
        let managed = Arc::new(ManagedImage {
            completion: watch::channel(None).0,
            record: Mutex::new(record),
            log,
        });
        {
            let mut images = lock(&self.inner.images);
            images.insert(image_id.clone(), Arc::clone(&managed));
        }
        self.inner.list.apply(api::ImageListMutation::Upsert(image));

        let inner = Arc::clone(&self.inner);
        let monitored = Arc::clone(&managed);
        let monitored_id = image_id.clone();
        let pods = pods.clone();
        let processes = processes.clone();
        tokio::spawn(async move {
            let monitor_inner = Arc::clone(&inner);
            let monitor_image = Arc::clone(&monitored);
            let monitor_id = monitored_id.clone();
            let task = tokio::spawn(generate_image(
                inner,
                monitored,
                monitored_id,
                input,
                writer,
                ImageGenerationServices {
                    pods,
                    processes,
                    network_service,
                    repositories,
                },
            ));
            if let Err(error) = task.await {
                warn!(%error, "image generation task failed");
                monitor_image.log.close();
                if let Err(finish_error) = finish_image(
                    &monitor_inner,
                    &monitor_image,
                    &monitor_id,
                    failed_state("image generation task failed"),
                    None,
                )
                .await
                {
                    warn!(%finish_error, "could not persist failed image generation");
                }
                monitor_image
                    .completion
                    .send_replace(Some(Err("image generation task failed".to_owned())));
            }
        });

        Ok(api::BuildImageOutput { image_id })
    }

    /// Resolves or admits the image generation required by a new pod.
    pub(crate) async fn image_for_pod(
        &self,
        pods: &PodService,
        processes: &ProcessSupervisor,
        network_service: Arc<GuestNetworkService>,
        input_refresh: &dyn ImageInputRefresh,
        repositories: Option<RepositoryPreparation>,
    ) -> Result<ImageForPod, Report<ImageServiceError>> {
        let input_directory = self
            .input_directory(input_refresh, repositories.as_ref())
            .await?;
        loop {
            let input = self.snapshot_input(input_directory.clone()).await?;
            let input_sha256 = api_input(&input).sha256;
            let current = {
                let images = lock(&self.inner.images);
                images
                    .values()
                    .filter_map(|image| {
                        let record = lock(&image.record);
                        (matches!(record.image.state, api::ImageState::Available)
                            && image_input_matches(&record.image.input, &input_sha256))
                        .then(|| {
                            record
                                .generation
                                .clone()
                                .map(|generation| (record.image.created_at, generation))
                        })
                        .flatten()
                    })
                    .max_by_key(|(created_at, _)| *created_at)
                    .map(|(_, generation)| generation)
            };
            if let Some(generation) = current {
                return Ok(ImageForPod::Available(generation));
            }

            let generating = {
                let images = lock(&self.inner.images);
                images.values().find(|image| image.is_generating()).cloned()
            };
            if let Some(generating) = generating {
                return Ok(ImageForPod::Building(pending_build(&generating)));
            }

            match self
                .build_current_input(
                    pods,
                    processes,
                    Arc::clone(&network_service),
                    repositories.clone(),
                    input_directory.clone(),
                )
                .await
            {
                Ok(output) => {
                    let image = lock(&self.inner.images)
                        .get(&output.image_id)
                        .cloned()
                        .ok_or_else(|| internal("admitted image disappeared"))?;
                    return Ok(ImageForPod::Building(pending_build(&image)));
                }
                Err(error) if matches!(error.error(), ImageServiceError::InvalidRequest(_)) => {}
                Err(error) => return Err(error),
            }
        }
    }

    /// Waits for one image build that was exposed through a pod state.
    pub(crate) async fn wait_for_pod_image(
        &self,
        mut build: PendingImageBuild,
    ) -> Result<RuntimeImageId, Report<ImageServiceError>> {
        loop {
            if let Some(completion) = build.completion.borrow().clone() {
                return completion
                    .map_err(|message| internal(format!("image generation failed: {message}")));
            }
            build
                .completion
                .changed()
                .await
                .map_err(|_| internal("image generation completion channel closed unexpectedly"))?;
        }
    }

    /// Reconciles one API image's canonical workspace seed without setup.
    #[tracing::instrument(
        name = "tascarrel_guest.images.update_workspace_seed",
        level = "info",
        skip_all,
        fields(image = ?input.image_id),
        err
    )]
    pub(crate) async fn update_workspace_seed(
        &self,
        input: api::UpdateImageWorkspaceSeedAction,
        repositories: Option<RepositoryPreparation>,
    ) -> Result<api::UpdateImageWorkspaceSeedOutput, Report<ImageServiceError>> {
        let generation = self.generation(&input.image_id).ok_or_else(|| {
            invalid_request("image is unavailable or has no canonical generation")
        })?;
        let updated = self
            .update_resolved_workspace_seed(&generation, repositories)
            .await?;
        Ok(api::UpdateImageWorkspaceSeedOutput { updated })
    }

    /// Reconciles one resolved image generation's canonical workspace seed.
    pub(crate) async fn update_resolved_workspace_seed(
        &self,
        generation: &RuntimeImageId,
        repositories: Option<RepositoryPreparation>,
    ) -> Result<bool, Report<ImageServiceError>> {
        let Some(repositories) = repositories else {
            return Ok(false);
        };
        repositories
            .update_image_seed(generation)
            .await
            .map_err(|error| internal(format!("update image workspace seed: {error}")))
    }

    /// Resolves one stable image definition directory for the operation.
    async fn input_directory(
        &self,
        input_refresh: &dyn ImageInputRefresh,
        repositories: Option<&RepositoryPreparation>,
    ) -> Result<PathBuf, Report<ImageServiceError>> {
        if let Some(directory) =
            repositories.and_then(RepositoryPreparation::image_definition_directory)
        {
            return Ok(directory.to_owned());
        }
        Ok(input_refresh
            .refresh_image_input()
            .await?
            .unwrap_or_else(|| self.inner.image_definition_directory.clone()))
    }

    /// Captures one immutable host-backed image input.
    async fn snapshot_input(
        &self,
        directory: PathBuf,
    ) -> Result<ImageInputSnapshot, Report<ImageServiceError>> {
        let limits = self.inner.input_limits;
        task::spawn_blocking(move || snapshot(&directory, limits))
            .await
            .map_err(|error| internal(format!("fingerprint image input task failed: {error}")))?
            .map_err(|error| {
                error.escalate(ImageServiceError::Internal(
                    "fingerprint image input failed".to_owned(),
                ))
            })
    }

    /// Returns the canonical Btrfs store directory owned by this service.
    #[must_use]
    pub fn store_directory(&self) -> &Path {
        self.inner.storage.root()
    }

    /// Returns the shared concrete store used by pod and repository features.
    #[must_use]
    pub fn storage(&self) -> Arc<BtrfsStore> {
        Arc::clone(&self.inner.storage)
    }

    /// Returns the immutable runtime generation published for one image.
    ///
    /// The result is absent while generation is running and after failure.
    #[must_use]
    pub fn generation(&self, image_id: &api::ImageId) -> Option<RuntimeImageId> {
        lock(&self.inner.images).get(image_id).and_then(|image| {
            let record = lock(&image.record);
            matches!(record.image.state, api::ImageState::Available)
                .then(|| record.generation.clone())
                .flatten()
        })
    }

    /// Opens a resumable subscription to the image list.
    ///
    /// # Errors
    ///
    /// Returns [`ImageServiceError::InvalidRequest`] when the cursor
    /// generation is not a UUID.
    pub(crate) fn subscribe_image_list(
        &self,
        input: &api::ImageListChangedSubscription,
    ) -> Result<ImageListSubscription, Report<ImageServiceError>> {
        let cursor = input.cursor.as_ref().map(runtime_stamp).transpose()?;
        Ok(self.inner.list.subscribe(cursor))
    }

    /// Opens a line-resumable subscription to one image generation log.
    ///
    /// # Errors
    ///
    /// Returns [`ImageServiceError::InvalidRequest`] when the image is
    /// unknown.
    pub(crate) fn subscribe_log(
        &self,
        input: api::ImageLogSubscription,
    ) -> Result<LogSubscription, Report<ImageServiceError>> {
        let api::ImageLogSubscription {
            image_id,
            last_line,
        } = input;
        let images = lock(&self.inner.images);
        let image = images.get(&image_id).ok_or_else(|| {
            invalid_request("image does not exist")
                .field_display("image_id", display_image_id(&image_id))
        })?;
        Ok(image.log.subscribe(last_line))
    }
}

/// Concrete filesystem and tool configuration for [`ImageService`].
#[derive(Clone, Debug)]
pub struct ImageServiceConfig {
    /// Host-backed workspace image configuration directory.
    pub image_definition_directory: PathBuf,
    /// Absolute iproute2 command used for the build network namespace.
    pub ip_program: PathBuf,
    /// Absolute `nsenter` command used by networking and the image builder.
    pub nsenter_program: PathBuf,
    /// Concrete `BuildKit` and OCI image builder configuration.
    pub builder: ImageBuilderConfig,
    /// Image-list mutations retained for subscription resumption.
    pub store_history_limit: NonZeroUsize,
    /// Sanitized lines retained for each image.
    pub log_capacity: NonZeroUsize,
    /// Maximum number of image log lines emitted in one event.
    pub log_batch_capacity: NonZeroUsize,
    /// Maximum UTF-8 byte length retained for one sanitized line.
    pub max_log_line_bytes: NonZeroUsize,
    /// Ordered workspace setup scripts executed before lifecycle hooks.
    pub setup_scripts: Vec<String>,
}

impl ImageServiceConfig {
    /// Creates concrete service configuration with default retention limits.
    #[must_use]
    pub fn new(
        image_definition_directory: impl Into<PathBuf>,
        ip_program: impl Into<PathBuf>,
        nsenter_program: impl Into<PathBuf>,
        builder: ImageBuilderConfig,
    ) -> Self {
        Self {
            image_definition_directory: image_definition_directory.into(),
            ip_program: ip_program.into(),
            nsenter_program: nsenter_program.into(),
            builder,
            store_history_limit: nonzero_default(1024),
            log_capacity: nonzero_default(2048),
            log_batch_capacity: nonzero_default(16),
            max_log_line_bytes: nonzero_default(16 * 1024),
            setup_scripts: Vec::new(),
        }
    }
}

/// Caller-relevant failure categories for the image service.
#[derive(Debug, Error)]
pub enum ImageServiceError {
    /// The requested operation violates the image lifecycle contract.
    #[error("invalid image request: {0}")]
    InvalidRequest(String),
    /// Image input, database, storage, build, or networking failed.
    #[error("image service operation failed: {0}")]
    Internal(String),
}

type ImageStore = Store<api::ImageList, api::ImageListMutation>;
pub(crate) type ImageListSubscription =
    tascarrel_store::Subscription<api::ImageList, api::ImageListMutation>;

/// Current result of resolving the image required by a new pod.
pub(crate) enum ImageForPod {
    /// An immutable generation can be used immediately.
    Available(RuntimeImageId),
    /// A generation must finish before image resolution can be retried.
    Building(PendingImageBuild),
}

/// Observable image generation currently blocking a pod creation.
pub(crate) struct PendingImageBuild {
    image_id: api::ImageId,
    completion: watch::Receiver<Option<Result<RuntimeImageId, String>>>,
}

impl PendingImageBuild {
    /// Returns the API image identifier used to subscribe to generation logs.
    pub(crate) fn image_id(&self) -> &api::ImageId {
        &self.image_id
    }
}

/// Client-safe diagnostic for one admitted generation failure.
#[derive(Debug, Error)]
#[error("image generation failed")]
struct ImageGenerationError {
    message: String,
}

impl ImageGenerationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: bounded_diagnostic(message.into()),
        }
    }

    fn message(&self) -> &str {
        &self.message
    }
}

struct ImageServiceInner {
    builder: Arc<ImageBuilder>,
    storage: Arc<BtrfsStore>,
    network: NetworkManager,
    build_network: PodNetwork,
    image_definition_directory: PathBuf,
    input_limits: ImageInputLimits,
    log_capacity: NonZeroUsize,
    log_batch_capacity: NonZeroUsize,
    max_log_line_bytes: NonZeroUsize,
    setup_scripts: Vec<String>,
    state: ImageStateRepository,
    images: Mutex<BTreeMap<api::ImageId, Arc<ManagedImage>>>,
    list: ImageStore,
    operation: tokio::sync::Mutex<()>,
}

struct ManagedImage {
    completion: watch::Sender<Option<Result<RuntimeImageId, String>>>,
    record: Mutex<ImageRecord>,
    log: LogBuffer,
}

impl ManagedImage {
    fn is_generating(&self) -> bool {
        matches!(lock(&self.record).image.state, api::ImageState::Generating)
    }
}

/// Creates a completion channel matching one recovered image state.
fn completion_channel(
    record: &ImageRecord,
) -> watch::Sender<Option<Result<RuntimeImageId, String>>> {
    let completion = match &record.image.state {
        api::ImageState::Available => record.generation.clone().map(Ok),
        api::ImageState::Failed(failure) => Some(Err(failure.message.to_string())),
        api::ImageState::Orphaned => Some(Err("image generation is orphaned".to_owned())),
        api::ImageState::Generated => Some(Err("image availability was not reconciled".to_owned())),
        api::ImageState::Generating => None,
    };
    watch::channel(completion).0
}

/// Creates the opaque pod-facing handle for one generating image.
fn pending_build(image: &Arc<ManagedImage>) -> PendingImageBuild {
    let image_id = lock(&image.record).image.id.clone();
    PendingImageBuild {
        image_id,
        completion: image.completion.subscribe(),
    }
}

/// Operation-time services retained by one asynchronous image generation.
struct ImageGenerationServices {
    pods: PodService,
    processes: ProcessSupervisor,
    network_service: Arc<GuestNetworkService>,
    repositories: Option<RepositoryPreparation>,
}

async fn generate_image(
    inner: Arc<ImageServiceInner>,
    image: Arc<ManagedImage>,
    image_id: api::ImageId,
    input: ImageInputSnapshot,
    log: ImageLogWriter,
    services: ImageGenerationServices,
) {
    log.write(
        &api::ImageLogSource::BuildKit,
        b"Starting workspace image build\n",
    );
    let generated = build_generation(&inner, &input, &log, &services).await;
    log.close();
    let completion = generated
        .as_ref()
        .map(|generation| (*generation).clone())
        .map_err(|error| error.error().message().to_owned());
    let (state, generation) = match generated {
        Ok(generation) => (api::ImageState::Available, Some(generation)),
        Err(error) => (failed_state(error.error().message()), None),
    };
    let completion = match finish_image(&inner, &image, &image_id, state, generation).await {
        Ok(()) => completion,
        Err(error) => {
            warn!(%error, "could not persist image generation outcome");
            Err("persisting image generation outcome failed".to_owned())
        }
    };
    image.completion.send_replace(Some(completion));
}

async fn build_generation(
    inner: &ImageServiceInner,
    input: &ImageInputSnapshot,
    log: &ImageLogWriter,
    services: &ImageGenerationServices,
) -> Result<RuntimeImageId, Report<ImageGenerationError>> {
    let principal = build_principal();
    inner
        .network
        .create_named(&inner.build_network, BUILD_NETWORK_NAMESPACE)
        .await
        .map_err(|error| generation_error(format!("create image build network: {error}")))?;
    let network_binding = match services
        .network_service
        .activate_build_veth(
            &principal,
            &inner.build_network.host_interface,
            inner.build_network.pod_address,
        )
        .await
    {
        Ok(network_binding) => network_binding,
        Err(error) => {
            let cleanup = inner
                .network
                .remove_named(&inner.build_network, BUILD_NETWORK_NAMESPACE)
                .await;
            return match cleanup {
                Ok(()) => Err(generation_error(format!(
                    "activate image build network service: {}",
                    error.message
                ))),
                Err(cleanup) => Err(generation_error(format!(
                    "activate image build network service: {}; network cleanup also failed: {cleanup}",
                    error.message
                ))),
            };
        }
    };

    let builder = Arc::clone(&inner.builder);
    let storage = Arc::clone(&inner.storage);
    let context = input.directory.clone();
    let expected_sha256 = input.sha256;
    let expected_modified_at = input.modified_at;
    let input_limits = inner.input_limits;
    let build_log = log.clone();
    let build = task::spawn_blocking(move || {
        build_and_publish(
            &builder,
            &storage,
            &context,
            input_limits,
            expected_sha256,
            expected_modified_at,
            build_log,
        )
    })
    .await;
    let build = match build {
        Ok(Ok(outcome)) => {
            let line = format!(
                "Published immutable generation {}\n",
                outcome.generation().id().as_str()
            );
            log.write(&api::ImageLogSource::BuildKit, line.as_bytes());
            Ok(outcome)
        }
        Ok(Err(error)) => {
            let message = error.error().message().to_owned();
            let line = format!("{message}\n");
            log.write(&api::ImageLogSource::BuildKit, line.as_bytes());
            Err(error)
        }
        Err(error) => Err(generation_error(format!(
            "image build task failed: {error}"
        ))),
    };

    let network_cleanup = inner
        .network
        .remove_named(&inner.build_network, BUILD_NETWORK_NAMESPACE)
        .await
        .map_err(|error| generation_error(format!("remove image build network: {error}")));
    let deactivation = match network_cleanup {
        Ok(()) => services
            .network_service
            .deactivate(&principal, &network_binding)
            .await
            .map_err(|error| {
                generation_error(format!(
                    "deactivate image build network service: {}",
                    error.message
                ))
            }),
        Err(error) => Err(error),
    };
    let outcome = resolve_build_cleanup(inner, build, deactivation).await?;
    prepare_built_generation(inner, input, outcome, log, services).await
}

async fn resolve_build_cleanup(
    inner: &ImageServiceInner,
    build: Result<ImageBuildOutcome, Report<ImageGenerationError>>,
    deactivation: Result<(), Report<ImageGenerationError>>,
) -> Result<ImageBuildOutcome, Report<ImageGenerationError>> {
    match (build, deactivation) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Err(error), Ok(())) => Err(error),
        (Ok(outcome), Err(cleanup)) if outcome.reused() => Err(cleanup),
        (Ok(outcome), Err(cleanup)) => {
            let generation = outcome.generation().id().clone();
            match discard_generation(inner, generation).await {
                Ok(()) => Err(cleanup),
                Err(discard) => Err(generation_error(format!(
                    "{}; image cleanup also failed: {}",
                    cleanup.error().message(),
                    discard.error().message()
                ))),
            }
        }
        (Err(build), Err(cleanup)) => Err(generation_error(format!(
            "{}; cleanup also failed: {}",
            build.error().message(),
            cleanup.error().message()
        ))),
    }
}

async fn prepare_built_generation(
    inner: &ImageServiceInner,
    input: &ImageInputSnapshot,
    outcome: ImageBuildOutcome,
    log: &ImageLogWriter,
    services: &ImageGenerationServices,
) -> Result<RuntimeImageId, Report<ImageGenerationError>> {
    let generation = outcome.generation().id().clone();

    match prepare_generation(inner, input, generation.clone(), log, services).await {
        Ok(()) => Ok(generation),
        Err(error) if outcome.reused() => Err(error),
        Err(error) => {
            let discarded = discard_generation(inner, generation).await;
            match discarded {
                Ok(()) => Err(error),
                Err(discard) => Err(generation_error(format!(
                    "{}; image cleanup also failed: {}",
                    error.error().message(),
                    discard.error().message()
                ))),
            }
        }
    }
}

/// Runs configured setup and lifecycle hooks in a hidden pod, commits its
/// prepared filesystems, and only then selects the base image generation.
async fn prepare_generation(
    inner: &ImageServiceInner,
    input: &ImageInputSnapshot,
    generation: RuntimeImageId,
    log: &ImageLogWriter,
    services: &ImageGenerationServices,
) -> Result<(), Report<ImageGenerationError>> {
    const SETUP_HOOKS: &str = r#"LC_ALL=C; export LC_ALL
if [ -n "${ZSH_VERSION:-}" ]; then setopt NULL_GLOB; fi
for hook in /run/tascarrel/hooks/setup/*; do
    [ ! -f "$hook" ] || "$0" -eu "$hook"
done"#;

    log.write(&api::ImageLogSource::Setup, b"Preparing golden workspace\n");
    if let Some(repositories) = services.repositories.as_ref() {
        repositories
            .reconcile_golden(&generation)
            .await
            .map_err(|error| generation_error(format!("reconcile golden workspace: {error}")))?;
    }
    log.write(
        &api::ImageLogSource::Setup,
        b"Starting workspace image setup\n",
    );
    let pod = services
        .pods
        .create_ephemeral(generation.clone(), &services.network_service)
        .await
        .map_err(|error| generation_error(format!("create image setup pod: {error}")))?;

    let setup = async {
        for (index, script) in inner.setup_scripts.iter().enumerate() {
            let number = index + 1;
            let marker = format!("Running configured setup step {number}\n");
            log.write(&api::ImageLogSource::Setup, marker.as_bytes());
            run_setup_process(
                &services.pods,
                &services.processes,
                &pod,
                format!("Image setup step {number}"),
                script.clone(),
                log,
            )
            .await?;
        }
        log.write(
            &api::ImageLogSource::Setup,
            b"Running workspace setup hooks\n",
        );
        run_setup_process(
            &services.pods,
            &services.processes,
            &pod,
            "Image setup hooks",
            SETUP_HOOKS,
            log,
        )
        .await?;

        let context = input_context(input)?;
        services
            .pods
            .commit_ephemeral(&pod, context, &services.network_service)
            .await
            .map_err(|error| generation_error(format!("commit image setup pod: {error}")))?;
        select_generation(inner, generation.clone()).await
    }
    .await;

    let cleanup = services
        .pods
        .destroy_ephemeral(pod, &services.network_service)
        .await;
    match (setup, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Ok(()), Err(error)) => {
            warn!(%error, "could not destroy committed image setup pod");
            Ok(())
        }
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup)) => Err(generation_error(format!(
            "{}; setup pod cleanup also failed: {cleanup}",
            error.error().message()
        ))),
    }
}

async fn run_setup_process(
    pods: &PodService,
    processes: &ProcessSupervisor,
    pod: &crate::services::pods::EphemeralPod,
    title: impl Into<tascarrel_api::ArcStr>,
    script: impl Into<tascarrel_api::ArcStr>,
    log: &ImageLogWriter,
) -> Result<(), Report<ImageGenerationError>> {
    let process = processes
        .spawn_setup(pods, pod, title, script)
        .map_err(|error| generation_error(format!("admit image setup process: {error}")))?;
    processes
        .wait_internal(process, |bytes| {
            log.write(&api::ImageLogSource::Setup, bytes);
        })
        .await
        .map_err(|error| generation_error(format!("run image setup process: {error}")))
}

async fn select_generation(
    inner: &ImageServiceInner,
    generation: RuntimeImageId,
) -> Result<(), Report<ImageGenerationError>> {
    let storage = Arc::clone(&inner.storage);
    task::spawn_blocking(move || storage.select_image(&generation))
        .await
        .map_err(|error| generation_error(format!("select image task failed: {error}")))?
        .map_err(|error| generation_error(format!("select generated image: {error}")))
}

async fn discard_generation(
    inner: &ImageServiceInner,
    generation: RuntimeImageId,
) -> Result<(), Report<ImageGenerationError>> {
    let storage = Arc::clone(&inner.storage);
    task::spawn_blocking(move || storage.remove_image(&generation))
        .await
        .map_err(|error| generation_error(format!("discard image task failed: {error}")))?
        .map_err(|error| generation_error(format!("discard failed image: {error}")))
}

fn build_and_publish(
    builder: &ImageBuilder,
    storage: &BtrfsStore,
    context: &Path,
    input_limits: ImageInputLimits,
    expected_sha256: [u8; 32],
    expected_modified_at: Timestamp,
    log: ImageLogWriter,
) -> Result<ImageBuildOutcome, Report<ImageGenerationError>> {
    let before = snapshot(context, input_limits)
        .map_err(|_| generation_error("image input could not be revalidated before building"))?;
    if before.sha256 != expected_sha256 || before.modified_at != expected_modified_at {
        return Err(generation_error(
            "image input changed before the build started",
        ));
    }
    let outcome = builder
        .build_with_output(storage, context, move |bytes| {
            log.write(&api::ImageLogSource::BuildKit, bytes);
        })
        .map_err(|error| generation_error(error.to_string()))?;
    let after = snapshot(context, input_limits)
        .map_err(|_| generation_error("image input could not be revalidated after building"))?;
    if after.sha256 != expected_sha256 || after.modified_at != expected_modified_at {
        let error = generation_error("image input changed while the image was being built");
        if outcome.reused() {
            return Err(error);
        }
        return match storage.remove_image(outcome.generation().id()) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(generation_error(format!(
                "{}; image cleanup also failed: {cleanup}",
                error.error().message()
            ))),
        };
    }
    Ok(outcome)
}

/// Deletes untracked generations and returns tracked non-root generations.
fn reconcile_storage(
    storage: &BtrfsStore,
    tracked: &BTreeSet<RuntimeImageId>,
) -> Result<BTreeSet<RuntimeImageId>, Report<StoreError>> {
    let generations = storage.list_images().report()?;
    let stored = generations
        .iter()
        .map(|generation| generation.id().clone())
        .collect::<BTreeSet<_>>();
    let available = generations
        .iter()
        .filter(|generation| generation.config().user().uid() != 0)
        .map(|generation| generation.id().clone())
        .collect::<BTreeSet<_>>();
    if storage
        .selected_image()
        .report()?
        .is_some_and(|selected| !tracked.contains(&selected) || !available.contains(&selected))
    {
        storage.clear_selected_image().report()?;
    }
    for generation in stored.difference(tracked) {
        storage.remove_image(generation).report()?;
    }
    Ok(available.intersection(tracked).cloned().collect())
}

async fn finish_image(
    inner: &ImageServiceInner,
    image: &Arc<ManagedImage>,
    image_id: &api::ImageId,
    state: api::ImageState,
    generation: Option<RuntimeImageId>,
) -> Result<(), Report<ImageServiceError>> {
    let changed = {
        let images = lock(&inner.images);
        let Some(current) = images.get(image_id) else {
            return Err(internal("generating image disappeared from the inventory"));
        };
        if !Arc::ptr_eq(current, image) {
            return Err(internal("generating image identity changed unexpectedly"));
        }
        let record = lock(&image.record);
        if !matches!(record.image.state, api::ImageState::Generating) {
            return Err(internal("image generation was already completed"));
        }
        let mut changed = record.clone();
        changed.image.state = state;
        changed.generation = generation;
        changed
    };
    inner
        .state
        .finish(changed.clone())
        .await
        .map_err(state_error)?;
    {
        let images = lock(&inner.images);
        let Some(current) = images.get(image_id) else {
            return Err(internal("persisted image disappeared from the inventory"));
        };
        if !Arc::ptr_eq(current, image) {
            return Err(internal("persisted image identity changed unexpectedly"));
        }
        let mut record = lock(&image.record);
        if !matches!(record.image.state, api::ImageState::Generating) {
            return Err(internal("persisted image was already completed in memory"));
        }
        *record = changed.clone();
    }
    inner
        .list
        .apply(api::ImageListMutation::Upsert(changed.image));
    Ok(())
}

fn failed_state(message: impl Into<tascarrel_api::ArcStr>) -> api::ImageState {
    api::ImageState::Failed(api::ImageGenerationFailure {
        message: message.into(),
        failed_at: Timestamp::now(),
    })
}

fn api_input(input: &ImageInputSnapshot) -> api::ImageInput {
    let encoded = encode_sha256(&input.sha256);
    let deserializer =
        serde::de::value::StringDeserializer::<serde::de::value::Error>::new(encoded);
    let sha256 = api::ImageInputSha256::deserialize(deserializer)
        .expect("Sidex string wrappers accept every string");
    api::ImageInput {
        sha256,
        modified_at: input.modified_at,
    }
}

fn image_input_matches(input: &api::ImageInput, sha256: &api::ImageInputSha256) -> bool {
    &input.sha256 == sha256
}

fn input_context(
    input: &ImageInputSnapshot,
) -> Result<RuntimeImageId, Report<ImageGenerationError>> {
    RuntimeImageId::new(format!("sha256:{}", encode_sha256(&input.sha256)))
        .map_err(|error| generation_error(format!("encode image input context: {error}")))
}

fn encode_sha256(sha256: &[u8; 32]) -> String {
    sha256
        .iter()
        .fold(String::with_capacity(64), |mut output, byte| {
            write!(output, "{byte:02x}").expect("writing to a String cannot fail");
            output
        })
}

fn reduce_image_list(list: &mut api::ImageList, mutation: &api::ImageListMutation) {
    match mutation {
        api::ImageListMutation::Upsert(image) => {
            if let Some(index) = list
                .images
                .iter()
                .position(|existing| existing.id == image.id)
            {
                list.images[index] = image.clone();
            } else {
                list.images.push(image.clone());
            }
        }
    }
}

fn build_principal() -> NetworkPrincipal {
    NetworkPrincipal {
        id: NetworkPrincipalId("workspace-image-build".into()),
        name: "workspace image build".into(),
        title: None,
        user: "root".into(),
        uid: 0,
        gid: 0,
        created_at_unix_ms: 0,
        health: Health::healthy(),
    }
}

fn runtime_stamp(
    stamp: &store_api::Stamp,
) -> Result<tascarrel_store::Stamp, Report<ImageServiceError>> {
    let generation = stamp.generation.parse::<Uuid>().map_err(|error| {
        ImageServiceError::InvalidRequest("image-list cursor generation is invalid".into())
            .report()
            .message(error.to_string())
    })?;
    Ok(tascarrel_store::Stamp {
        generation,
        version: stamp.version,
    })
}

async fn blocking<T, E, F>(
    operation: &'static str,
    function: F,
) -> Result<T, Report<ImageServiceError>>
where
    T: Send + 'static,
    E: std::fmt::Display + Send + 'static,
    F: FnOnce() -> Result<T, E> + Send + 'static,
{
    task::spawn_blocking(function)
        .await
        .map_err(|error| internal(format!("{operation} task failed: {error}")))?
        .map_err(|error| internal(format!("{operation} failed: {error}")))
}

fn display_image_id(image_id: &api::ImageId) -> tascarrel_api::ArcStr {
    image_id.0.clone()
}

/// Converts a statically non-zero service default.
fn nonzero_default(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("service default is statically non-zero")
}

fn invalid_request(message: impl Into<String>) -> Report<ImageServiceError> {
    ImageServiceError::InvalidRequest(message.into()).report()
}

fn internal(message: impl Into<String>) -> Report<ImageServiceError> {
    ImageServiceError::Internal(message.into()).report()
}

fn state_error(error: Report<ImageStateError>) -> Report<ImageServiceError> {
    error.escalate(ImageServiceError::Internal(
        "access image state database failed".to_owned(),
    ))
}

fn generation_error(message: impl Into<String>) -> Report<ImageGenerationError> {
    ImageGenerationError::new(message).report()
}

fn bounded_diagnostic(message: String) -> String {
    const MAX_CHARS: usize = 2048;
    const HEAD_CHARS: usize = 512;
    let count = message.chars().count();
    if count <= MAX_CHARS {
        return message;
    }
    let head = message.chars().take(HEAD_CHARS).collect::<String>();
    let tail = message
        .chars()
        .skip(count - (MAX_CHARS - HEAD_CHARS))
        .collect::<String>();
    format!("{head}\n... diagnostic truncated ...\n{tail}")
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr as _;

    use super::*;

    /// Verifies image reuse follows the authoritative digest, not the mtime
    /// hint.
    #[test]
    fn image_input_matching_uses_only_sha256() {
        let original = ImageInputSnapshot {
            directory: PathBuf::from("/image"),
            sha256: [7; 32],
            modified_at: Timestamp::from_str("2026-01-01T00:00:00Z")
                .expect("fixture timestamp is valid"),
        };
        let input = api_input(&original);
        let same_content_new_mtime = ImageInputSnapshot {
            modified_at: Timestamp::from_str("2026-02-01T00:00:00Z")
                .expect("fixture timestamp is valid"),
            ..original.clone()
        };
        let changed_content = ImageInputSnapshot {
            sha256: [8; 32],
            ..same_content_new_mtime.clone()
        };

        assert!(image_input_matches(
            &input,
            &api_input(&same_content_new_mtime).sha256
        ));
        assert!(!image_input_matches(
            &input,
            &api_input(&changed_content).sha256
        ));
    }
}
