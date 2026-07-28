//! Pod lifecycle ownership and resumable state.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::future::Future;
use std::io::IoSliceMut;
use std::io::Write as _;
use std::mem::MaybeUninit;
use std::num::NonZeroUsize;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr as _;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::Weak;
use std::time::Duration;

use jiff::Timestamp;
use nix::sys::socket::getsockopt;
use nix::sys::socket::sockopt::PeerCredentials;
use reportify::ErrorExt as _;
use reportify::Report;
use rustix::fs::Mode;
use rustix::fs::OFlags;
use rustix::fs::open;
use tascarrel_api::types::pods as api;
use tascarrel_api::types::processes::ProcessId;
use tascarrel_api::types::store as store_api;
use tascarrel_protocol::Health;
use tascarrel_protocol::Pod as NetworkPrincipal;
use tascarrel_protocol::PodId as NetworkPodId;
use tascarrel_store::Store;
use thiserror::Error;
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio::io::unix::AsyncFd;
use tokio::net::UnixListener;
use tokio::net::UnixStream;
use tokio::sync::Notify;
use tokio::sync::OwnedMutexGuard;
use tokio::sync::mpsc;
use tokio::task;
use tracing::warn;

use super::nix_roots::NixRoots;
use super::runc::PodExecution;
use super::runc::PreparedReadiness;
use super::runc::Runc;
use super::runc::RuncConfig;
use super::state::PersistentPodState;
use super::state::PodRecord;
use super::state::PodStateError;
use super::state::PodStateRepository;
use crate::ChatService;
use crate::Database;
use crate::GuestNetworkService;
use crate::NetworkBinding;
use crate::NetworkManager;
use crate::PodNetwork;
use crate::repositories::RepositoryPreparation;
use crate::runtime::pod::BtrfsStore;
use crate::runtime::pod::ImageId;
use crate::runtime::pod::PodDevice;
use crate::runtime::pod::PodId as RuntimePodId;
use crate::runtime::pod::PodStorage;
use crate::runtime::pod::StoreError;
use crate::services::images::ImageForPod;
use crate::services::images::ImageInputRefresh;
use crate::services::images::ImageService;
use crate::services::processes::ProcessSupervisor;

const ERROR_DETAIL_LIMIT: usize = 1024;
const MAX_INIT_SCRIPT_BYTES: usize = 64 * 1024;
const POD_READY_TIMEOUT: Duration = Duration::from_secs(90);
const POD_READY_ACK: u8 = 1;

/// Concrete workspace pod lifecycle service.
#[derive(Clone)]
pub struct PodService {
    inner: Arc<PodServiceInner>,
}

impl PodService {
    /// Opens durable pod state and reconciles transient runc state to stopped.
    ///
    /// # Errors
    ///
    /// Returns an internal report when state, storage, runc, or networking
    /// cannot be reconciled safely.
    pub async fn open(
        config: PodServiceConfig,
        database: Database,
        storage: Arc<BtrfsStore>,
        network: NetworkManager,
    ) -> Result<Self, Report<PodServiceError>> {
        let nix_root = config.runc.nix_gc_root_dir.clone();
        let nix_trash = config.runc.nix_gc_root_trash_dir.clone();
        let nix_enabled = config.runc.policy.nix_daemon();
        let state = PodStateRepository::new(database);
        let nix_roots = Arc::new(
            blocking("open pod Nix GC roots", move || {
                NixRoots::open(nix_root, nix_trash)
            })
            .await?,
        );
        let runc = Arc::new(
            Runc::new(config.runc)
                .map_err(|error| internal(format!("initialize runc: {error}")))?,
        );
        let records = recover(&state, &storage, &runc, &network, &nix_roots, nix_enabled).await?;
        let pods = records
            .values()
            .filter(|record| !record.ephemeral)
            .map(|record| record.pod.clone())
            .collect();
        let store = Store::new(
            api::PodList { pods },
            reduce_pod_list,
            config.store_history_limit,
        );
        let (pod_controls, control_connections) = mpsc::channel(64);
        Ok(Self {
            inner: Arc::new(PodServiceInner {
                state,
                storage,
                runc,
                network,
                nix_roots,
                nix_enabled,
                init_steps: config.init_steps,
                pending: Mutex::new(BTreeMap::new()),
                pending_creation_changes: Notify::new(),
                records: Mutex::new(records),
                running: Mutex::new(BTreeMap::new()),
                operations: Mutex::new(BTreeMap::new()),
                startups: Mutex::new(BTreeMap::new()),
                store,
                pod_controls,
                control_connections: Mutex::new(Some(control_connections)),
            }),
        })
    }

    /// Takes the stream of authenticated listeners created by running pods.
    ///
    /// # Errors
    ///
    /// Returns a typed error if the single-consumer stream was already taken.
    pub fn take_control_connections(
        &self,
    ) -> Result<mpsc::Receiver<PodControlConnection>, Report<PodServiceError>> {
        lock(&self.inner.control_connections)
            .take()
            .ok_or_else(|| invalid("pod control connection stream was already taken"))
    }

    /// Creates a pod asynchronously and returns its observable identifier.
    ///
    /// # Errors
    ///
    /// Returns a contract report for an invalid title or an internal report
    /// when its initial runtime state cannot be created. Image generation
    /// and storage failures are reported through the pod list afterwards.
    #[tracing::instrument(level = "debug", skip_all)]
    pub fn create(
        &self,
        input: api::CreatePodAction,
        images: &ImageService,
        processes: &ProcessSupervisor,
        network_service: Arc<GuestNetworkService>,
        image_input: Arc<dyn ImageInputRefresh>,
    ) -> Result<api::CreatePodOutput, Report<PodServiceError>> {
        self.create_with_repository_preparation_task(
            input,
            images,
            processes,
            network_service,
            image_input,
            async { Ok(None) },
        )
    }

    /// Creates a pod while resolving repository inputs in the background.
    pub(crate) fn create_with_repository_preparation_task<F>(
        &self,
        input: api::CreatePodAction,
        images: &ImageService,
        processes: &ProcessSupervisor,
        network_service: Arc<GuestNetworkService>,
        image_input: Arc<dyn ImageInputRefresh>,
        repositories: F,
    ) -> Result<api::CreatePodOutput, Report<PodServiceError>>
    where
        F: Future<Output = Result<Option<RepositoryPreparation>, Report<PodServiceError>>>
            + Send
            + 'static,
    {
        let pod_id = api::PodId::generate();
        let title = input.title.map_or_else(
            || Ok(generated_title(&pod_id)),
            |title| validate_title(&title).map(|()| title),
        )?;
        let pod = api::Pod {
            id: pod_id.clone(),
            title,
            status: api::PodState::Creating,
            created_at: Timestamp::now(),
        };
        lock(&self.inner.pending).insert(pod_id.clone(), pod.clone());
        self.inner.store.apply(api::PodListMutation::Upsert(pod));

        let service = self.clone();
        let images = images.clone();
        let processes = processes.clone();
        let created_pod_id = pod_id.clone();
        tokio::spawn(async move {
            let monitored_service = service.clone();
            let monitored_pod_id = created_pod_id.clone();
            let creation = tokio::spawn(async move {
                let repositories = repositories.await?;
                service
                    .complete_creation(
                        &created_pod_id,
                        &images,
                        &processes,
                        &network_service,
                        image_input.as_ref(),
                        repositories,
                    )
                    .await
            });
            let failure = match creation.await {
                Ok(Ok(())) => return,
                Ok(Err(error)) => error.to_string(),
                Err(error) => format!("pod creation task failed: {error}"),
            };
            if let Err(error) = monitored_service
                .fail_pending_creation(&monitored_pod_id, failure)
                .await
            {
                warn!(pod_id = %monitored_pod_id.0, %error, "could not persist pod creation failure");
            }
        });
        Ok(api::CreatePodOutput { pod_id })
    }

    /// Resolves the current image and materializes one created pod.
    async fn complete_creation(
        &self,
        pod_id: &api::PodId,
        images: &ImageService,
        processes: &ProcessSupervisor,
        network_service: &Arc<GuestNetworkService>,
        image_input: &dyn ImageInputRefresh,
        repositories: Option<RepositoryPreparation>,
    ) -> Result<(), Report<PodServiceError>> {
        let generation = loop {
            match images
                .image_for_pod(
                    self,
                    processes,
                    Arc::clone(network_service),
                    image_input,
                    repositories.clone(),
                )
                .await
                .map_err(|error| internal(format!("resolve pod image: {error}")))?
            {
                ImageForPod::Available(generation) => break generation,
                ImageForPod::Building(build) => {
                    let status = api::PodState::Building(api::PodImageBuild {
                        image_id: build.image_id().clone(),
                    });
                    if !self.transition_pending_creation(pod_id, status).await? {
                        return Ok(());
                    }
                    images
                        .wait_for_pod_image(build)
                        .await
                        .map_err(|error| internal(format!("build pod image: {error}")))?;
                    if !self
                        .transition_pending_creation(pod_id, api::PodState::Creating)
                        .await?
                    {
                        return Ok(());
                    }
                }
            }
        };

        images
            .update_resolved_workspace_seed(&generation, repositories)
            .await
            .map_err(|error| internal(format!("update pod image workspace seed: {error}")))?;

        let _operation = self.pod_operation(pod_id).lock_owned().await;
        let Some(mut pod) = lock(&self.inner.pending).get(pod_id).cloned() else {
            return Ok(());
        };
        pod.status = api::PodState::Creating;
        let record = self
            .inner
            .state
            .create(pod, generation.clone(), false)
            .await
            .map_err(state_error)?;
        lock(&self.inner.pending).remove(pod_id);
        self.inner.pending_creation_changes.notify_waiters();
        self.publish_record(record.clone());
        self.materialize_record(record, generation, false)
            .await
            .map(|_| ())
    }

    /// Publishes a pending creation state unless the pod was destroyed.
    async fn transition_pending_creation(
        &self,
        pod_id: &api::PodId,
        status: api::PodState,
    ) -> Result<bool, Report<PodServiceError>> {
        let _operation = self.pod_operation(pod_id).lock_owned().await;
        let Some(mut pod) = lock(&self.inner.pending).get(pod_id).cloned() else {
            return Ok(false);
        };
        pod.status = status;
        lock(&self.inner.pending).insert(pod_id.clone(), pod.clone());
        self.inner.store.apply(api::PodListMutation::Upsert(pod));
        Ok(true)
    }

    /// Converts a still-pending background creation into a runtime failure.
    async fn fail_pending_creation(
        &self,
        pod_id: &api::PodId,
        message: String,
    ) -> Result<(), Report<PodServiceError>> {
        let _operation = self.pod_operation(pod_id).lock_owned().await;
        let Some(mut pod) = lock(&self.inner.pending).get(pod_id).cloned() else {
            return Ok(());
        };
        pod.status = api::PodState::Failed(api::PodFailure {
            message: message.into(),
            failed_at: Timestamp::now(),
        });
        lock(&self.inner.pending).insert(pod_id.clone(), pod.clone());
        self.inner.store.apply(api::PodListMutation::Upsert(pod));
        self.inner.pending_creation_changes.notify_waiters();
        Ok(())
    }

    /// Creates and starts a durable ephemeral pod from an explicit image.
    ///
    /// The returned pod is intentionally absent from [`api::PodList`]. If
    /// guestd restarts before the caller destroys it, recovery removes its
    /// runtime, storage, Nix roots, and active database record.
    pub(crate) async fn create_ephemeral(
        &self,
        image: ImageId,
        network_service: &Arc<GuestNetworkService>,
    ) -> Result<EphemeralPod, Report<PodServiceError>> {
        let pod_id = api::PodId::generate();
        let _operation = self.pod_operation(&pod_id).lock_owned().await;
        let pod = EphemeralPod {
            pod_id: pod_id.clone(),
            setup_shell: self.inner.runc.setup_shell().to_owned(),
        };
        let title = format!("Image setup {}", pod_id.0).into();
        let created = async {
            let mut record = self.create_record(pod_id, title, image, true).await?;
            self.start_stopped_record(&mut record, network_service)
                .await?;
            Ok::<(), Report<PodServiceError>>(())
        }
        .await;
        match created {
            Ok(()) => Ok(pod),
            Err(error) => {
                let cleanup_required = lock(&self.inner.records).contains_key(pod.id());
                if cleanup_required
                    && let Err(cleanup) = self.destroy_ephemeral_locked(&pod, network_service).await
                {
                    warn!(pod_id = %pod.id().0, %cleanup, "could not clean failed ephemeral pod");
                }
                Err(error)
            }
        }
    }

    /// Starts one stopped pod through runc.
    ///
    /// # Errors
    ///
    /// Returns a contract report for an unknown or active pod and an internal
    /// report when its runtime resources cannot be started.
    #[tracing::instrument(level = "debug", skip_all, fields(pod_id = %input.pod_id.0))]
    pub async fn start(
        &self,
        input: api::StartPodAction,
        processes: &ProcessSupervisor,
        network_service: &Arc<GuestNetworkService>,
    ) -> Result<api::StartPodOutput, Report<PodServiceError>> {
        let operation = self.pod_operation(&input.pod_id);
        let operation = operation.lock_owned().await;
        let mut record = self.public_record(&input.pod_id)?;
        if !matches!(record.pod.status, api::PodState::Stopped) {
            return Err(invalid("pod is not stopped"));
        }
        self.start_public_record(&mut record, processes, network_service, operation)
            .await?;
        Ok(api::StartPodOutput {})
    }

    /// Waits until asynchronous creation has produced durable pod storage.
    pub(crate) async fn wait_until_created(
        &self,
        pod_id: &api::PodId,
    ) -> Result<(), Report<PodServiceError>> {
        loop {
            let creation_changed = self.inner.pending_creation_changes.notified();
            tokio::pin!(creation_changed);
            creation_changed.as_mut().enable();
            let operation = self.pod_operation(pod_id).lock_owned().await;
            let pending = lock(&self.inner.pending).get(pod_id).cloned();
            match pending.map(|pod| pod.status) {
                None => {
                    self.public_record(pod_id)?;
                    return Ok(());
                }
                Some(api::PodState::Failed(failure)) => {
                    return Err(internal(format!(
                        "pod creation failed: {}",
                        failure.message
                    )));
                }
                Some(_) => {
                    drop(operation);
                    creation_changed.await;
                }
            }
        }
    }

    /// Ensures one pod is running for an operation that supports implicit
    /// startup.
    ///
    /// # Errors
    ///
    /// Returns a contract report for an unknown pod or a pod in a state other
    /// than running or stopped, and an internal report when startup fails.
    pub(crate) async fn ensure_running(
        &self,
        pod_id: &api::PodId,
        processes: &ProcessSupervisor,
        network_service: &Arc<GuestNetworkService>,
    ) -> Result<(), Report<PodServiceError>> {
        self.wait_until_created(pod_id).await?;
        let operation = self.pod_operation(pod_id);
        let operation = operation.lock_owned().await;
        let mut record = self.public_record(pod_id)?;
        match &record.pod.status {
            api::PodState::Running => Ok(()),
            api::PodState::Stopped => {
                self.start_public_record(&mut record, processes, network_service, operation)
                    .await
            }
            api::PodState::Initializing(_) => {
                let startup = lock(&self.inner.startups)
                    .get(pod_id)
                    .cloned()
                    .ok_or_else(|| internal("initializing pod has no startup completion signal"))?;
                let completed = startup.notified();
                tokio::pin!(completed);
                completed.as_mut().enable();
                drop(operation);
                completed.await;

                let _operation = self.pod_operation(pod_id).lock_owned().await;
                let record = self.public_record(pod_id)?;
                if matches!(record.pod.status, api::PodState::Running) {
                    Ok(())
                } else {
                    Err(invalid("pod startup was interrupted"))
                }
            }
            _ => Err(invalid("pod cannot be started from its current state")),
        }
    }

    /// Stops one pod while retaining its storage and durable state.
    ///
    /// # Errors
    ///
    /// Returns a contract report for an unknown pod and an internal report
    /// when runc or network cleanup fails.
    #[tracing::instrument(
        level = "debug",
        skip(self, network_service),
        fields(pod_id = %input.pod_id.0)
    )]
    pub async fn stop(
        &self,
        input: api::StopPodAction,
        network_service: &Arc<GuestNetworkService>,
    ) -> Result<api::StopPodOutput, Report<PodServiceError>> {
        let _operation = self.pod_operation(&input.pod_id).lock_owned().await;
        let mut record = self.public_record(&input.pod_id)?;
        if matches!(record.pod.status, api::PodState::Stopped) {
            return Ok(api::StopPodOutput {});
        }
        record.pod.status = api::PodState::Stopping;
        self.publish_record(record.clone());
        self.finish_startup(&input.pod_id);
        let running = lock(&self.inner.running).get(&input.pod_id).cloned();
        if let Err(error) = self
            .stop_record(&record, running.as_ref(), network_service)
            .await
        {
            self.mark_failed(&mut record, error.to_string());
            return Err(error);
        }
        lock(&self.inner.running).remove(&input.pod_id);
        record.pod.status = api::PodState::Stopped;
        self.publish_record(record);
        Ok(api::StopPodOutput {})
    }

    /// Destroys one pod and all persistent resources owned by it.
    ///
    /// # Errors
    ///
    /// Returns a contract report for an unknown pod and an internal report
    /// when runtime, state persistence, or storage cleanup fails.
    #[tracing::instrument(
        level = "debug",
        skip(self, chats, network_service),
        fields(pod_id = %input.pod_id.0)
    )]
    pub async fn destroy(
        &self,
        input: api::DestroyPodAction,
        chats: &ChatService,
        network_service: &Arc<GuestNetworkService>,
    ) -> Result<api::DestroyPodOutput, Report<PodServiceError>> {
        let _operation = self.pod_operation(&input.pod_id).lock_owned().await;
        if !lock(&self.inner.pending).contains_key(&input.pod_id) {
            self.public_record(&input.pod_id)?;
        }
        chats
            .archive_pod_chats(&input.pod_id)
            .await
            .map_err(|error| internal(format!("archive pod chats: {error}")))?;
        if lock(&self.inner.pending).remove(&input.pod_id).is_some() {
            self.inner.pending_creation_changes.notify_waiters();
            self.inner
                .store
                .apply(api::PodListMutation::Remove(input.pod_id.clone()));
            self.finish_startup(&input.pod_id);
            return Ok(api::DestroyPodOutput {});
        }
        let mut record = self.public_record(&input.pod_id)?;
        record.persistent_state = PersistentPodState::Destroying;
        record.pod.status = api::PodState::Destroying;
        self.commit_persistent_record(record.clone()).await?;
        self.finish_startup(&input.pod_id);
        let running = lock(&self.inner.running).get(&input.pod_id).cloned();
        self.stop_record(&record, running.as_ref(), network_service)
            .await?;
        lock(&self.inner.running).remove(&input.pod_id);
        self.withdraw_nix_roots(&input.pod_id).await?;
        let storage = Arc::clone(&self.inner.storage);
        let runtime_id = runtime_id(&input.pod_id)?;
        blocking("destroy pod storage", move || {
            match storage.destroy_pod(&runtime_id) {
                Ok(()) | Err(StoreError::PodNotFound(_)) => Ok(()),
                Err(error) => Err(error),
            }
        })
        .await?;
        self.archive_record(input.pod_id.clone()).await?;
        self.inner
            .store
            .apply(api::PodListMutation::Remove(input.pod_id.clone()));
        Ok(api::DestroyPodOutput {})
    }

    /// Stops an ephemeral pod and freezes its root and workspace as the setup
    /// seed for `context`.
    pub(crate) async fn commit_ephemeral(
        &self,
        pod: &EphemeralPod,
        context: ImageId,
        network_service: &Arc<GuestNetworkService>,
    ) -> Result<String, Report<PodServiceError>> {
        let _operation = self.pod_operation(pod.id()).lock_owned().await;
        let mut record = self.ephemeral_record(pod)?;
        if !matches!(record.pod.status, api::PodState::Stopped) {
            record.pod.status = api::PodState::Stopping;
            self.publish_record(record.clone());
            let running = lock(&self.inner.running).get(pod.id()).cloned();
            if let Err(error) = self
                .stop_record(&record, running.as_ref(), network_service)
                .await
            {
                self.mark_failed(&mut record, error.to_string());
                return Err(error);
            }
            lock(&self.inner.running).remove(pod.id());
            record.pod.status = api::PodState::Stopped;
            self.publish_record(record);
        }
        let storage = Arc::clone(&self.inner.storage);
        let runtime_id = runtime_id(pod.id())?;
        blocking("commit ephemeral pod storage", move || {
            storage.publish_setup_seed(&runtime_id, &context)
        })
        .await
    }

    /// Destroys an ephemeral pod and archives its durable lifecycle record.
    pub(crate) async fn destroy_ephemeral(
        &self,
        pod: EphemeralPod,
        network_service: &Arc<GuestNetworkService>,
    ) -> Result<(), Report<PodServiceError>> {
        let _operation = self.pod_operation(pod.id()).lock_owned().await;
        self.destroy_ephemeral_locked(&pod, network_service).await
    }

    /// Destroys one validated ephemeral pod while the operation lock is held.
    async fn destroy_ephemeral_locked(
        &self,
        pod: &EphemeralPod,
        network_service: &Arc<GuestNetworkService>,
    ) -> Result<(), Report<PodServiceError>> {
        let mut record = self.ephemeral_record(pod)?;
        record.persistent_state = PersistentPodState::Destroying;
        record.pod.status = api::PodState::Destroying;
        self.commit_persistent_record(record.clone()).await?;
        let running = lock(&self.inner.running).get(pod.id()).cloned();
        self.stop_record(&record, running.as_ref(), network_service)
            .await?;
        lock(&self.inner.running).remove(pod.id());
        self.withdraw_nix_roots(pod.id()).await?;
        let storage = Arc::clone(&self.inner.storage);
        let runtime_id = runtime_id(pod.id())?;
        blocking("destroy ephemeral pod storage", move || {
            match storage.destroy_pod(&runtime_id) {
                Ok(()) | Err(StoreError::PodNotFound(_)) => Ok(()),
                Err(error) => Err(error),
            }
        })
        .await?;
        self.archive_record(pod.pod_id.clone()).await
    }

    /// Resolves process execution coordinates without checking runtime state.
    ///
    /// # Errors
    ///
    /// Returns a contract report for an unknown pod and an internal report for
    /// malformed durable storage or execution state.
    pub(crate) async fn execution(
        &self,
        pod_id: &api::PodId,
    ) -> Result<PodExecution, Report<PodServiceError>> {
        let record = self.public_record(pod_id)?;
        self.execution_for_record(pod_id, &record).await
    }

    /// Locks one pod while a chat's durable state is created.
    pub(crate) async fn lock_for_chat_creation(
        &self,
        pod_id: &api::PodId,
    ) -> Result<OwnedMutexGuard<()>, Report<PodServiceError>> {
        let operation = self.pod_operation(pod_id).lock_owned().await;
        if lock(&self.inner.pending).contains_key(pod_id) {
            return Ok(operation);
        }
        let record = self.public_record(pod_id)?;
        if record.persistent_state == PersistentPodState::Destroying {
            return Err(invalid("pod is being destroyed"));
        }
        Ok(operation)
    }

    /// Resolves execution coordinates for an opaque ephemeral pod handle.
    pub(crate) async fn ephemeral_execution(
        &self,
        pod: &EphemeralPod,
    ) -> Result<PodExecution, Report<PodServiceError>> {
        let record = self.ephemeral_record(pod)?;
        self.execution_for_record(pod.id(), &record).await
    }

    async fn execution_for_record(
        &self,
        pod_id: &api::PodId,
        record: &PodRecord,
    ) -> Result<PodExecution, Report<PodServiceError>> {
        let storage = self.storage(pod_id).await?;
        self.inner
            .runc
            .execution(&storage, record.slot)
            .map_err(|error| internal(format!("resolve pod execution: {error}")))
    }

    /// Opens a resumable subscription to the pod list.
    ///
    /// # Errors
    ///
    /// Returns a contract report when the cursor generation is not a UUID.
    pub(crate) fn subscribe(
        &self,
        input: &api::PodListChangedSubscription,
    ) -> Result<PodListSubscription, Report<PodServiceError>> {
        let cursor = input.cursor.as_ref().map(runtime_stamp).transpose()?;
        Ok(self.inner.store.subscribe(cursor))
    }

    /// Returns the current public pod inventory.
    pub(crate) fn pod_snapshot(&self) -> Vec<api::Pod> {
        self.inner.store.snapshot().value.pods.to_vec()
    }

    /// Updates one pod's human-readable title.
    pub(crate) async fn set_title(
        &self,
        pod_id: &api::PodId,
        title: tascarrel_api::ArcStr,
    ) -> Result<api::Pod, Report<PodServiceError>> {
        validate_title(&title)?;
        let _operation = self.pod_operation(pod_id).lock_owned().await;
        if let Some(mut pod) = lock(&self.inner.pending).get(pod_id).cloned() {
            pod.title = title;
            lock(&self.inner.pending).insert(pod_id.clone(), pod.clone());
            self.inner
                .store
                .apply(api::PodListMutation::Upsert(pod.clone()));
            return Ok(pod);
        }
        let mut record = self.public_record(pod_id)?;
        record.pod.title = title;
        self.write_record(record.clone()).await?;
        let pod = record.pod.clone();
        self.publish_record(record);
        Ok(pod)
    }

    /// Replaces a pod title when it still matches an expected value.
    pub(crate) async fn replace_title(
        &self,
        pod_id: &api::PodId,
        expected: &str,
        title: tascarrel_api::ArcStr,
    ) -> Result<bool, Report<PodServiceError>> {
        validate_title(&title)?;
        let _operation = self.pod_operation(pod_id).lock_owned().await;
        if let Some(mut pod) = lock(&self.inner.pending).get(pod_id).cloned() {
            if pod.title.as_ref() != expected || pod.title == title {
                return Ok(false);
            }
            pod.title = title;
            lock(&self.inner.pending).insert(pod_id.clone(), pod.clone());
            self.inner.store.apply(api::PodListMutation::Upsert(pod));
            return Ok(true);
        }
        let mut record = self.public_record(pod_id)?;
        if record.pod.title.as_ref() != expected || record.pod.title == title {
            return Ok(false);
        }
        record.pod.title = title;
        self.write_record(record.clone()).await?;
        self.publish_record(record);
        Ok(true)
    }

    /// Resolves the persistent workspace for one pod.
    pub(crate) async fn workspace_root(
        &self,
        pod_id: &api::PodId,
    ) -> Result<PathBuf, Report<PodServiceError>> {
        self.public_record(pod_id)?;
        let storage = self.storage(pod_id).await?;
        Ok(storage.workspace().to_path_buf())
    }

    /// Returns the pinned idmapped workspace mount for a running pod.
    pub(crate) fn active_workspace_watch(
        &self,
        pod_id: &api::PodId,
    ) -> Result<Option<Arc<OwnedFd>>, Report<PodServiceError>> {
        self.public_record(pod_id)?;
        Ok(lock(&self.inner.running)
            .get(pod_id)
            .map(|running| Arc::clone(&running.workspace_watch)))
    }

    /// Replaces USB devices exposed to all running pods.
    ///
    /// # Errors
    ///
    /// Returns an internal report when a running runc container rejects the
    /// updated device set.
    pub async fn replace_devices(
        &self,
        devices: Vec<PodDevice>,
    ) -> Result<(), Report<PodServiceError>> {
        let runc = Arc::clone(&self.inner.runc);
        blocking("store pod devices", move || runc.store_devices(devices)).await?;
        let pod_ids = lock(&self.inner.running)
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        for pod_id in pod_ids {
            let _operation = self.pod_operation(&pod_id).lock_owned().await;
            if !lock(&self.inner.running).contains_key(&pod_id) {
                continue;
            }
            let Some(record) = lock(&self.inner.records).get(&pod_id).cloned() else {
                continue;
            };
            let runtime_id = runtime_id(&pod_id)?;
            let slot = record.slot;
            let runc = Arc::clone(&self.inner.runc);
            blocking("replace pod devices", move || {
                runc.sync_current_devices(&runtime_id, slot)
            })
            .await?;
        }
        Ok(())
    }

    /// Stops every running pod during orderly guestd shutdown.
    pub async fn shutdown(&self, network_service: &Arc<GuestNetworkService>) {
        let ids = lock(&self.inner.records)
            .values()
            .filter(|record| !record.ephemeral)
            .map(|record| record.pod.id.clone())
            .collect::<Vec<_>>();
        for pod_id in ids {
            if let Err(error) = self
                .stop(
                    api::StopPodAction {
                        pod_id: pod_id.clone(),
                    },
                    network_service,
                )
                .await
            {
                warn!(pod_id = %pod_id.0, %error, "could not stop pod during shutdown");
            }
        }
    }

    /// Opens one TCP connection to a loopback port in a running pod.
    ///
    /// The returned stream is connected to podd's private control socket and
    /// has completed podd's port-selection handshake.
    ///
    /// # Errors
    ///
    /// Returns a contract report for an invalid target or a pod that is not
    /// running, and an internal report for a failed podd handshake.
    #[tracing::instrument(level = "debug", skip(self), fields(pod_id = %pod_id.0, pod_port))]
    pub async fn connect_port(
        &self,
        pod_id: &NetworkPodId,
        pod_port: u16,
    ) -> Result<UnixStream, Report<PodServiceError>> {
        if pod_port == 0 {
            return Err(invalid("pod port must not be zero"));
        }
        let pod_id = api::PodId::from_str(&pod_id.0)
            .map_err(|error| invalid(format!("invalid pod id: {error}")))?;
        let _operation = self.pod_operation(&pod_id).lock_owned().await;
        let control = lock(&self.inner.running)
            .get(&pod_id)
            .map(|running| running.podd_control.clone())
            .ok_or_else(|| invalid("pod is not running"))?;
        let mut podd = UnixStream::connect(&control).await.map_err(|error| {
            internal(format!("failed to connect to pod control socket: {error}"))
        })?;
        podd.write_all(&pod_port.to_be_bytes())
            .await
            .map_err(|error| internal(format!("failed to select pod loopback port: {error}")))?;
        let status = podd.read_u8().await.map_err(|error| {
            internal(format!(
                "failed to read pod loopback connection result: {error}"
            ))
        })?;
        if status != 0 {
            return Err(internal("pod loopback port refused the connection"));
        }
        Ok(podd)
    }

    /// Starts the concrete runc, namespace, and network-service resources for
    /// one record.
    #[allow(clippy::too_many_lines)] // Pod startup keeps fail-closed resource cleanup in one scope.
    async fn start_record(
        &self,
        record: &PodRecord,
        network_service: &Arc<GuestNetworkService>,
    ) -> Result<RunningPod, Report<PodServiceError>> {
        let runtime_id = runtime_id(&record.pod.id)?;
        let storage_id = runtime_id.clone();
        let pod_storage = Arc::clone(&self.inner.storage);
        let storage = blocking("reset pod temporary storage", move || {
            pod_storage.reset_pod_temporary(&storage_id)
        })
        .await?;
        self.provision_nix_roots(record, &storage).await?;
        let runc = Arc::clone(&self.inner.runc);
        let prepared_storage = storage.clone();
        let slot = record.slot;
        let prepared = blocking("prepare pod with runc", move || {
            runc.prepare(&prepared_storage, slot)
        })
        .await?;
        let prepared_pid = prepared.pid;
        let readiness = match PodReadiness::new(prepared.readiness) {
            Ok(readiness) => readiness,
            Err(error) => return Err(self.rollback_prepared(record, error).await),
        };
        let network = match PodNetwork::for_slot(record.slot) {
            Ok(network) => network,
            Err(error) => {
                drop(readiness);
                return Err(self
                    .rollback_prepared(record, internal(format!("allocate pod network: {error}")))
                    .await);
            }
        };
        if let Err(error) = self.inner.network.create(&network, prepared_pid).await {
            drop(readiness);
            self.rollback_start(record, &network, network_service, None)
                .await;
            return Err(internal(format!("create pod network: {error}")));
        }
        let execution = match self.inner.runc.execution(&storage, record.slot) {
            Ok(execution) => execution,
            Err(error) => {
                drop(readiness);
                self.rollback_start(record, &network, network_service, None)
                    .await;
                return Err(internal(format!("resolve pod execution: {error}")));
            }
        };
        let principal = network_principal(record, &execution, storage.created_at_unix_ms());
        let network_binding = match network_service
            .activate_veth(&principal, &network.host_interface, network.pod_address)
            .await
        {
            Ok(network_binding) => network_binding,
            Err(error) => {
                drop(readiness);
                self.rollback_start(record, &network, network_service, None)
                    .await;
                return Err(internal(format!(
                    "activate pod network service: {}",
                    error.message
                )));
            }
        };
        let runc = Arc::clone(&self.inner.runc);
        let readiness_id = runtime_id.clone();
        let slot = record.slot;
        if let Err(error) =
            blocking("start pod with runc", move || runc.start(&runtime_id, slot)).await
        {
            drop(readiness);
            let error = self.attach_startup_log(error, record).await;
            self.rollback_start(
                record,
                &network,
                network_service,
                Some((&principal, &network_binding)),
            )
            .await;
            return Err(error);
        }
        let workspace_watch = match self
            .wait_for_podd_ready(readiness_id, record.slot, readiness)
            .await
        {
            Ok(workspace_watch) => workspace_watch,
            Err(error) => {
                let error = self.attach_startup_log(error, record).await;
                self.rollback_start(
                    record,
                    &network,
                    network_service,
                    Some((&principal, &network_binding)),
                )
                .await;
                return Err(error);
            }
        };
        let podd_control = PathBuf::from(format!(
            "/proc/{prepared_pid}/root/run/tascarrel/podd-control.sock"
        ));
        let listener = match acquire_pod_control_listener(podd_control.clone()).await {
            Ok(listener) => listener,
            Err(error) => {
                let error = self.attach_startup_log(error, record).await;
                self.rollback_start(
                    record,
                    &network,
                    network_service,
                    Some((&principal, &network_binding)),
                )
                .await;
                return Err(error);
            }
        };
        if self
            .inner
            .pod_controls
            .send(PodControlConnection {
                pod_id: record.pod.id.clone(),
                listener,
            })
            .await
            .is_err()
        {
            let error = self
                .attach_startup_log(
                    internal("pod control listener consumer is unavailable"),
                    record,
                )
                .await;
            self.rollback_start(
                record,
                &network,
                network_service,
                Some((&principal, &network_binding)),
            )
            .await;
            return Err(error);
        }
        Ok(RunningPod {
            principal,
            network,
            network_binding,
            workspace_watch: Arc::new(workspace_watch),
            podd_control,
        })
    }

    /// Authenticates podd's event-driven readiness connection, verifies the
    /// same runc init is still running, and acknowledges the attempt.
    async fn wait_for_podd_ready(
        &self,
        runtime_id: RuntimePodId,
        slot: u32,
        readiness: PodReadiness,
    ) -> Result<OwnedFd, Report<PodServiceError>> {
        let PodReadiness {
            listener,
            pidfd,
            init_pid,
            mapped_user,
            mapped_group,
            handshake,
            _cleanup,
        } = readiness;
        let connection =
            accept_podd_readiness(&listener, init_pid, mapped_user, mapped_group, &handshake);
        tokio::pin!(connection);
        let mut stream = tokio::select! {
            result = &mut connection => result?,
            result = pidfd.readable() => {
                let _exited = result
                    .map_err(|error| internal(format!("wait for pod init exit: {error}")))?;
                return Err(internal("pod init exited before reporting readiness"));
            }
            () = tokio::time::sleep(POD_READY_TIMEOUT) => {
                return Err(internal(format!(
                    "podd did not connect to guestd within {POD_READY_TIMEOUT:?}"
                )));
            }
        };

        let runc = Arc::clone(&self.inner.runc);
        blocking("confirm ready pod with runc", move || {
            runc.confirm_ready(&runtime_id, slot, init_pid)
        })
        .await?;
        let workspace_path = PathBuf::from(format!("/proc/{init_pid}/root/workspace"));
        let workspace_watch = blocking("open running pod workspace", move || {
            open(
                &workspace_path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )
        })
        .await?;
        stream
            .write_all(&[POD_READY_ACK])
            .await
            .map_err(|error| internal(format!("acknowledge pod readiness: {error}")))?;
        Ok(workspace_watch)
    }

    /// Rolls back a runc create which failed before network ownership began.
    async fn rollback_prepared(
        &self,
        record: &PodRecord,
        cause: Report<PodServiceError>,
    ) -> Report<PodServiceError> {
        let runtime_id = match runtime_id(&record.pod.id) {
            Ok(runtime_id) => runtime_id,
            Err(error) => return internal(format!("{cause}; rollback also failed: {error}")),
        };
        let runc = Arc::clone(&self.inner.runc);
        let slot = record.slot;
        match blocking("roll back prepared pod", move || {
            runc.stop(&runtime_id, slot)
        })
        .await
        {
            Ok(()) => cause,
            Err(rollback) => internal(format!("{cause}; rollback also failed: {rollback}")),
        }
    }

    /// Attaches the bounded podd startup log before failed-start rollback
    /// removes the transient runtime bundle.
    async fn attach_startup_log(
        &self,
        error: Report<PodServiceError>,
        record: &PodRecord,
    ) -> Report<PodServiceError> {
        let runc = Arc::clone(&self.inner.runc);
        let runtime_id = match runtime_id(&record.pod.id) {
            Ok(runtime_id) => runtime_id,
            Err(log_error) => {
                return internal(format!(
                    "{error}; could not resolve runtime startup log: {log_error}"
                ));
            }
        };
        let slot = record.slot;
        match blocking("read pod startup log", move || {
            runc.startup_log(&runtime_id, slot)
        })
        .await
        {
            Ok(log) if !log.is_empty() => internal(startup_failure_detail(
                &error.to_string(),
                &String::from_utf8_lossy(&log),
            )),
            Ok(_) => error,
            Err(log_error) => internal(format!(
                "{error}; could not read pod startup log: {log_error}"
            )),
        }
    }

    /// Creates durable storage for one ordinary or ephemeral pod while its
    /// pod-scoped operation lock is held.
    async fn create_record(
        &self,
        pod_id: api::PodId,
        title: tascarrel_api::ArcStr,
        image: ImageId,
        ephemeral: bool,
    ) -> Result<PodRecord, Report<PodServiceError>> {
        let pod = api::Pod {
            id: pod_id,
            title,
            status: api::PodState::Creating,
            created_at: Timestamp::now(),
        };
        let record = self
            .inner
            .state
            .create(pod, image.clone(), ephemeral)
            .await
            .map_err(state_error)?;
        self.publish_record(record.clone());

        self.materialize_record(record, image, ephemeral).await
    }

    /// Creates storage for one already-persisted record.
    async fn materialize_record(
        &self,
        mut record: PodRecord,
        image: ImageId,
        setup_basis: bool,
    ) -> Result<PodRecord, Report<PodServiceError>> {
        let runtime_id = runtime_id(&record.pod.id)?;

        let storage = Arc::clone(&self.inner.storage);
        let created = blocking("create pod storage", move || {
            if setup_basis {
                storage.create_setup_pod(runtime_id, &image)
            } else {
                storage.create_pod(runtime_id, &image)
            }
        })
        .await;
        let storage = match created {
            Ok(storage) => storage,
            Err(error) => {
                self.mark_failed(&mut record, error.to_string());
                return Err(error);
            }
        };
        if let Err(error) = self.provision_nix_roots(&record, &storage).await {
            self.mark_failed(&mut record, error.to_string());
            return Err(error);
        }
        record.persistent_state = PersistentPodState::Ready;
        record.pod.status = api::PodState::Stopped;
        self.commit_persistent_record(record.clone()).await?;
        Ok(record)
    }

    /// Starts an ephemeral record known to be stopped.
    async fn start_stopped_record(
        &self,
        record: &mut PodRecord,
        network_service: &Arc<GuestNetworkService>,
    ) -> Result<(), Report<PodServiceError>> {
        record.pod.status = api::PodState::Starting;
        self.publish_record(record.clone());
        match self.start_record(record, network_service).await {
            Ok(running) => {
                lock(&self.inner.running).insert(record.pod.id.clone(), running);
                if let Err(error) = self.sync_current_devices(record).await {
                    self.fail_started_record(record, error.to_string(), network_service)
                        .await?;
                    return Err(error);
                }
                record.pod.status = api::PodState::Running;
                self.publish_record(record.clone());
                Ok(())
            }
            Err(error) => {
                self.mark_failed(record, error.to_string());
                Err(error)
            }
        }
    }

    /// Starts one ordinary pod and releases its operation lock while waiting
    /// for readiness-blocking initialization processes.
    async fn start_public_record(
        &self,
        record: &mut PodRecord,
        processes: &ProcessSupervisor,
        network_service: &Arc<GuestNetworkService>,
        operation: OwnedMutexGuard<()>,
    ) -> Result<(), Report<PodServiceError>> {
        record.pod.status = api::PodState::Starting;
        self.publish_record(record.clone());
        let running = match self.start_record(record, network_service).await {
            Ok(running) => running,
            Err(error) => {
                self.mark_failed(record, error.to_string());
                return Err(error);
            }
        };
        lock(&self.inner.running).insert(record.pod.id.clone(), running);
        if let Err(error) = self.sync_current_devices(record).await {
            self.fail_started_record(record, error.to_string(), network_service)
                .await?;
            return Err(error);
        }

        let blocking_processes = match self.spawn_init(record, processes) {
            Ok(processes) => processes,
            Err(error) => {
                self.fail_started_record(record, error.to_string(), network_service)
                    .await?;
                return Err(error);
            }
        };
        if blocking_processes.is_empty() {
            record.pod.status = api::PodState::Running;
            self.publish_record(record.clone());
            return Ok(());
        }

        let startup = Arc::new(Notify::new());
        lock(&self.inner.startups).insert(record.pod.id.clone(), Arc::clone(&startup));
        let pod_id = record.pod.id.clone();
        let canceled = startup.notified();
        tokio::pin!(canceled);
        canceled.as_mut().enable();
        drop(operation);
        let initialization = tokio::select! {
            result = self.wait_for_init(processes, blocking_processes) => Some(result),
            () = &mut canceled => None,
        };

        let _operation = self.pod_operation(&pod_id).lock_owned().await;
        let is_current = lock(&self.inner.startups)
            .get(&pod_id)
            .is_some_and(|current| Arc::ptr_eq(current, &startup));
        if !is_current {
            return Err(invalid("pod startup was interrupted"));
        }
        let Some(mut current) = lock(&self.inner.records).get(&pod_id).cloned() else {
            self.finish_startup(&pod_id);
            return Err(invalid("pod startup was interrupted"));
        };
        if !matches!(current.pod.status, api::PodState::Initializing(_)) {
            self.finish_startup(&pod_id);
            return Err(invalid("pod startup was interrupted"));
        }
        match initialization.expect("current startup was not canceled") {
            Ok(()) => {
                current.pod.status = api::PodState::Running;
                self.publish_record(current);
                self.finish_startup(&pod_id);
                Ok(())
            }
            Err(error) => {
                let message = error.to_string();
                let cleanup = self
                    .fail_started_record(&mut current, message, network_service)
                    .await;
                self.finish_startup(&pod_id);
                cleanup?;
                Err(error)
            }
        }
    }

    /// Starts configured init processes and returns the readiness blockers.
    fn spawn_init(
        &self,
        record: &mut PodRecord,
        processes: &ProcessSupervisor,
    ) -> Result<Vec<ProcessId>, Report<PodServiceError>> {
        let mut blocking_processes = Vec::new();
        let mut initialization_processes = Vec::new();
        for (index, step) in self.inner.init_steps.iter().enumerate() {
            let process_id = processes
                .spawn_init(
                    self,
                    record.pod.id.clone(),
                    self.inner.runc.setup_shell(),
                    index + 1,
                    step.script(),
                )
                .map_err(|error| internal(format!("start pod init process: {error}")))?;
            if step.wait() {
                blocking_processes.push(process_id.clone());
            }
            initialization_processes.push(api::PodInitializationProcess {
                process_id,
                wait: step.wait(),
            });
        }
        if blocking_processes.is_empty() {
            return Ok(blocking_processes);
        }
        record.pod.status = api::PodState::Initializing(api::PodInitialization {
            processes: initialization_processes.into(),
        });
        self.publish_record(record.clone());
        Ok(blocking_processes)
    }

    /// Waits for all readiness-blocking initialization processes.
    async fn wait_for_init(
        &self,
        processes: &ProcessSupervisor,
        blocking_processes: Vec<ProcessId>,
    ) -> Result<(), Report<PodServiceError>> {
        for process_id in blocking_processes {
            processes
                .wait_for_success(&process_id)
                .await
                .map_err(|error| internal(format!("wait for pod init process: {error}")))?;
        }
        Ok(())
    }

    /// Stops runtime resources after startup fails and publishes the failure.
    async fn fail_started_record(
        &self,
        record: &mut PodRecord,
        mut message: String,
        network_service: &Arc<GuestNetworkService>,
    ) -> Result<(), Report<PodServiceError>> {
        let running = lock(&self.inner.running).get(&record.pod.id).cloned();
        if let Err(error) = self
            .stop_record(record, running.as_ref(), network_service)
            .await
        {
            message.push_str("; cleanup failed: ");
            message.push_str(&error.to_string());
            self.mark_failed(record, message);
            return Err(error);
        }
        lock(&self.inner.running).remove(&record.pod.id);
        self.mark_failed(record, message);
        Ok(())
    }

    /// Applies the latest workspace device set after one pod becomes running.
    async fn sync_current_devices(
        &self,
        record: &PodRecord,
    ) -> Result<(), Report<PodServiceError>> {
        let runc = Arc::clone(&self.inner.runc);
        let runtime_id = runtime_id(&record.pod.id)?;
        let slot = record.slot;
        blocking("synchronize started pod devices", move || {
            runc.sync_current_devices(&runtime_id, slot)
        })
        .await
    }

    /// Releases transient resources for one pod without touching storage.
    async fn stop_record(
        &self,
        record: &PodRecord,
        running: Option<&RunningPod>,
        network_service: &Arc<GuestNetworkService>,
    ) -> Result<(), Report<PodServiceError>> {
        let runtime_id = runtime_id(&record.pod.id)?;
        let mut failures = Vec::new();
        if running.is_some() || self.inner.runc.has_local_state(&runtime_id) {
            let runc = Arc::clone(&self.inner.runc);
            let slot = record.slot;
            if let Err(error) =
                blocking("stop pod with runc", move || runc.stop(&runtime_id, slot)).await
            {
                failures.push(error.to_string());
            }
        }
        if let Some(running) = running {
            if let Err(error) = self.inner.network.remove(&running.network).await {
                failures.push(format!("remove pod network: {error}"));
            }
            if let Err(error) = network_service
                .deactivate(&running.principal, &running.network_binding)
                .await
            {
                failures.push(format!("deactivate pod network service: {}", error.message));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(internal(failures.join("; ")))
        }
    }

    /// Best-effort rollback for a partially started runtime.
    async fn rollback_start(
        &self,
        record: &PodRecord,
        network: &PodNetwork,
        guest_network: &Arc<GuestNetworkService>,
        network_binding: Option<(&NetworkPrincipal, &NetworkBinding)>,
    ) {
        if let Ok(runtime_id) = runtime_id(&record.pod.id) {
            let runc = Arc::clone(&self.inner.runc);
            let slot = record.slot;
            if let Err(error) = blocking("roll back pod runtime", move || {
                runc.stop(&runtime_id, slot)
            })
            .await
            {
                warn!(pod_id = %record.pod.id.0, %error, "could not roll back pod runtime");
            }
        }
        if let Err(error) = self.inner.network.remove(network).await {
            warn!(pod_id = %record.pod.id.0, %error, "could not roll back pod network");
        }
        if let Some((principal, network_binding)) = network_binding
            && let Err(error) = guest_network.deactivate(principal, network_binding).await
        {
            warn!(pod_id = %record.pod.id.0, %error, "could not roll back pod network service");
        }
    }

    /// Returns one durable record or a contract error.
    fn public_record(&self, pod_id: &api::PodId) -> Result<PodRecord, Report<PodServiceError>> {
        lock(&self.inner.records)
            .get(pod_id)
            .filter(|record| !record.ephemeral)
            .cloned()
            .ok_or_else(|| invalid("pod does not exist"))
    }

    /// Returns a record only when it belongs to the supplied ephemeral handle.
    fn ephemeral_record(&self, pod: &EphemeralPod) -> Result<PodRecord, Report<PodServiceError>> {
        lock(&self.inner.records)
            .get(pod.id())
            .filter(|record| record.ephemeral)
            .cloned()
            .ok_or_else(|| internal("ephemeral pod does not exist"))
    }

    /// Resolves one pod's Btrfs storage without consulting runtime state.
    async fn storage(&self, pod_id: &api::PodId) -> Result<PodStorage, Report<PodServiceError>> {
        let runtime_id = runtime_id(pod_id)?;
        let storage = Arc::clone(&self.inner.storage);
        blocking("read pod storage", move || storage.pod(&runtime_id)).await
    }

    /// Persists a recovery-state transition and publishes the current API
    /// state.
    async fn commit_persistent_record(
        &self,
        record: PodRecord,
    ) -> Result<(), Report<PodServiceError>> {
        self.write_record(record.clone()).await?;
        self.publish_record(record);
        Ok(())
    }

    /// Replaces one runtime-only record and publishes its list mutation.
    fn publish_record(&self, record: PodRecord) {
        lock(&self.inner.records).insert(record.pod.id.clone(), record.clone());
        if !record.ephemeral {
            self.inner
                .store
                .apply(api::PodListMutation::Upsert(record.pod));
        }
    }

    /// Persists one record in the guest database.
    async fn write_record(&self, record: PodRecord) -> Result<(), Report<PodServiceError>> {
        self.inner.state.save(record).await.map_err(state_error)
    }

    /// Archives one record and removes it from the active in-memory collection.
    async fn archive_record(&self, pod_id: api::PodId) -> Result<(), Report<PodServiceError>> {
        self.inner
            .state
            .archive(pod_id.clone(), Timestamp::now())
            .await
            .map_err(state_error)?;
        lock(&self.inner.records).remove(&pod_id);
        Ok(())
    }

    /// Publishes a runtime lifecycle failure without changing recovery state.
    fn mark_failed(&self, record: &mut PodRecord, message: String) {
        record.pod.status = api::PodState::Failed(api::PodFailure {
            message: message.into(),
            failed_at: Timestamp::now(),
        });
        self.publish_record(record.clone());
    }

    /// Creates or validates one pod's Nix direct-root directories.
    async fn provision_nix_roots(
        &self,
        record: &PodRecord,
        storage: &PodStorage,
    ) -> Result<(), Report<PodServiceError>> {
        if !self.inner.nix_enabled {
            return self.withdraw_nix_roots(&record.pod.id).await;
        }
        let execution = self
            .inner
            .runc
            .execution(storage, record.slot)
            .map_err(|error| internal(format!("resolve pod identity: {error}")))?;
        let roots = Arc::clone(&self.inner.nix_roots);
        let pod_id = runtime_id(&record.pod.id)?;
        blocking("provision pod Nix GC roots", move || {
            roots.provision(&pod_id, execution.uid, execution.gid)
        })
        .await
    }

    /// Withdraws one pod's Nix direct roots from the scanned tree.
    async fn withdraw_nix_roots(&self, pod_id: &api::PodId) -> Result<(), Report<PodServiceError>> {
        let roots = Arc::clone(&self.inner.nix_roots);
        let pod_id = runtime_id(pod_id)?;
        blocking("withdraw pod Nix GC roots", move || roots.withdraw(&pod_id)).await
    }

    /// Returns the operation lock scoped to one pod identity.
    fn pod_operation(&self, pod_id: &api::PodId) -> Arc<tokio::sync::Mutex<()>> {
        let mut operations = lock(&self.inner.operations);
        operations.retain(|_, operation| operation.strong_count() > 0);
        if let Some(operation) = operations.get(pod_id).and_then(Weak::upgrade) {
            return operation;
        }
        let operation = Arc::new(tokio::sync::Mutex::new(()));
        operations.insert(pod_id.clone(), Arc::downgrade(&operation));
        operation
    }

    /// Completes or cancels an in-flight initialization wait.
    fn finish_startup(&self, pod_id: &api::PodId) {
        if let Some(startup) = lock(&self.inner.startups).remove(pod_id) {
            startup.notify_waiters();
        }
    }
}

/// Configuration for one workspace pod service.
#[derive(Clone, Debug)]
pub struct PodServiceConfig {
    /// Concrete runc configuration used for every pod.
    pub runc: RuncConfig,
    /// Number of pod-list mutations retained for resumption.
    pub store_history_limit: NonZeroUsize,
    /// Ordered initialization steps launched whenever an ordinary pod starts.
    pub init_steps: Vec<PodInitStep>,
}

impl PodServiceConfig {
    /// Creates a configuration with the default mutation history limit.
    #[must_use]
    pub fn new(runc: RuncConfig) -> Self {
        Self {
            runc,
            store_history_limit: nonzero_default(1024),
            init_steps: Vec::new(),
        }
    }
}

/// One shell step run as the image user whenever an ordinary pod starts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PodInitStep {
    script: String,
    wait: bool,
}

impl PodInitStep {
    /// Creates a bounded initialization step.
    ///
    /// # Errors
    ///
    /// Returns an error when the script is too large or contains a NUL byte.
    pub fn new(script: impl Into<String>, wait: bool) -> Result<Self, Report<PodInitStepError>> {
        let script = script.into();
        if script.len() > MAX_INIT_SCRIPT_BYTES || script.contains('\0') {
            return Err(PodInitStepError::InvalidScript.report());
        }
        Ok(Self { script, wait })
    }

    /// Returns the shell source for this step.
    #[must_use]
    pub fn script(&self) -> &str {
        &self.script
    }

    /// Returns whether pod readiness waits for this step to succeed.
    #[must_use]
    pub const fn wait(&self) -> bool {
        self.wait
    }
}

/// Failure from the pod feature service.
#[derive(Debug, Error)]
pub enum PodServiceError {
    /// The requested operation violates the pod lifecycle contract.
    #[error("invalid pod request: {0}")]
    InvalidRequest(String),
    /// Pod infrastructure failed unexpectedly.
    #[error("pod service failed: {0}")]
    Internal(String),
}

/// Failure while validating one pod initialization step.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum PodInitStepError {
    /// The script exceeded the size bound or contained a NUL byte.
    #[error("invalid pod initialization step: script exceeds 64 KiB or contains a NUL byte")]
    InvalidScript,
}

type PodStore = Store<api::PodList, api::PodListMutation>;
pub(crate) type PodListSubscription =
    tascarrel_store::Subscription<api::PodList, api::PodListMutation>;

/// Opaque handle to a durable pod used only by guestd infrastructure.
///
/// Internal pods are excluded from the public pod list and are removed during
/// startup recovery if guestd exits before their owner destroys them.
#[derive(Clone, Debug)]
pub(crate) struct EphemeralPod {
    pod_id: api::PodId,
    setup_shell: PathBuf,
}

impl EphemeralPod {
    /// Returns the pod identifier for another internal guestd feature.
    pub(crate) fn id(&self) -> &api::PodId {
        &self.pod_id
    }

    /// Returns the immutable Nix-store Bash injected into the pod.
    pub(crate) fn setup_shell(&self) -> &Path {
        &self.setup_shell
    }
}

struct PodServiceInner {
    state: PodStateRepository,
    storage: Arc<BtrfsStore>,
    runc: Arc<Runc>,
    network: NetworkManager,
    nix_roots: Arc<NixRoots>,
    nix_enabled: bool,
    init_steps: Vec<PodInitStep>,
    pending: Mutex<BTreeMap<api::PodId, api::Pod>>,
    pending_creation_changes: Notify,
    records: Mutex<BTreeMap<api::PodId, PodRecord>>,
    running: Mutex<BTreeMap<api::PodId, RunningPod>>,
    operations: Mutex<BTreeMap<api::PodId, Weak<tokio::sync::Mutex<()>>>>,
    startups: Mutex<BTreeMap<api::PodId, Arc<Notify>>>,
    store: PodStore,
    pod_controls: mpsc::Sender<PodControlConnection>,
    control_connections: Mutex<Option<mpsc::Receiver<PodControlConnection>>>,
}

/// Authenticated pod-local listener handed from podd to guestd.
pub struct PodControlConnection {
    /// Pod which owns every connection accepted by this listener.
    pub pod_id: api::PodId,
    /// Listener visible at the fixed pod control path.
    pub listener: UnixListener,
}

#[derive(Clone)]
struct RunningPod {
    principal: NetworkPrincipal,
    network: PodNetwork,
    network_binding: NetworkBinding,
    workspace_watch: Arc<OwnedFd>,
    podd_control: PathBuf,
}

/// Async views of one prepared per-pod readiness endpoint.
struct PodReadiness {
    listener: UnixListener,
    pidfd: AsyncFd<OwnedFd>,
    init_pid: u32,
    mapped_user: u32,
    mapped_group: u32,
    handshake: Vec<u8>,
    _cleanup: PreparedReadiness,
}

impl PodReadiness {
    fn new(mut prepared: PreparedReadiness) -> Result<Self, Report<PodServiceError>> {
        let raw_pid = i32::try_from(prepared.pid)
            .map_err(|_| internal("prepared pod init PID exceeds Linux pid_t"))?;
        let pid = rustix::process::Pid::from_raw(raw_pid)
            .ok_or_else(|| internal("prepared pod init PID is zero"))?;
        let pidfd = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::NONBLOCK)
            .map_err(|error| internal(format!("open pod init pidfd: {error}")))?;
        let pidfd = AsyncFd::new(pidfd)
            .map_err(|error| internal(format!("register pod init pidfd: {error}")))?;
        let listener = prepared
            .take_listener()
            .map_err(|error| internal(format!("take pod readiness listener: {error}")))?;
        let listener = UnixListener::from_std(listener)
            .map_err(|error| internal(format!("register pod readiness listener: {error}")))?;
        Ok(Self {
            listener,
            pidfd,
            init_pid: prepared.pid,
            mapped_user: prepared.uid,
            mapped_group: prepared.gid,
            handshake: prepared.handshake.clone(),
            _cleanup: prepared,
        })
    }
}

/// Accepts only the expected outer PID and mapped-root identity. Linux
/// translates `SO_PEERCRED` into the listener's PID namespace, so guestd sees
/// the same outer PID reported by runc rather than podd's nested PID 1.
async fn accept_podd_readiness(
    listener: &UnixListener,
    init_pid: u32,
    mapped_user: u32,
    mapped_group: u32,
    handshake: &[u8],
) -> Result<UnixStream, Report<PodServiceError>> {
    loop {
        let (mut stream, _) = listener
            .accept()
            .await
            .map_err(|error| internal(format!("accept pod readiness connection: {error}")))?;
        let credentials = getsockopt(&stream, PeerCredentials)
            .map_err(|error| internal(format!("inspect pod readiness peer: {error}")))?;
        if u32::try_from(credentials.pid()).ok() != Some(init_pid)
            || credentials.uid() != mapped_user
            || credentials.gid() != mapped_group
        {
            continue;
        }
        let mut received = vec![0_u8; handshake.len()];
        stream
            .read_exact(&mut received)
            .await
            .map_err(|error| internal(format!("read authenticated pod readiness: {error}")))?;
        if received != handshake {
            return Err(internal(
                "authenticated pod readiness handshake did not match this attempt",
            ));
        }
        return Ok(stream);
    }
}

/// Requests the pod-local listener and receives its descriptor over
/// `SCM_RIGHTS`.
#[tracing::instrument(level = "debug", skip_all, fields(path = %path.display()), err)]
async fn acquire_pod_control_listener(
    path: PathBuf,
) -> Result<UnixListener, Report<PodServiceError>> {
    task::spawn_blocking(move || {
        let mut control = std::os::unix::net::UnixStream::connect(&path)
            .map_err(|error| internal(format!("connect pod control listener handoff: {error}")))?;
        control
            .write_all(&0_u16.to_be_bytes())
            .map_err(|error| internal(format!("request pod control listener: {error}")))?;
        let mut payload = [0_u8; 1];
        let mut slices = [IoSliceMut::new(&mut payload)];
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
        let mut ancillary = rustix::net::RecvAncillaryBuffer::new(&mut space);
        rustix::net::recvmsg(
            &control,
            &mut slices,
            &mut ancillary,
            rustix::net::RecvFlags::CMSG_CLOEXEC,
        )
        .map_err(|error| internal(format!("receive pod control listener: {error}")))?;
        let mut descriptor = None;
        for message in ancillary.drain() {
            if let rustix::net::RecvAncillaryMessage::ScmRights(descriptors) = message {
                for received in descriptors {
                    if descriptor.replace(received).is_some() {
                        return Err(internal("pod control handoff returned multiple listeners"));
                    }
                }
            }
        }
        let descriptor =
            descriptor.ok_or_else(|| internal("pod control handoff returned no listener"))?;
        let listener = std::os::unix::net::UnixListener::from(descriptor);
        listener
            .set_nonblocking(true)
            .map_err(|error| internal(format!("configure pod control listener: {error}")))?;
        UnixListener::from_std(listener)
            .map_err(|error| internal(format!("register pod control listener: {error}")))
    })
    .await
    .map_err(|error| internal(format!("pod control listener handoff task failed: {error}")))?
}

/// Reconciles durable records and Btrfs storage after guestd starts.
#[allow(clippy::too_many_lines)] // Recovery keeps cross-store reconciliation decisions together.
async fn recover(
    state: &PodStateRepository,
    storage: &Arc<BtrfsStore>,
    runc: &Arc<Runc>,
    network: &NetworkManager,
    nix_roots: &Arc<NixRoots>,
    nix_enabled: bool,
) -> Result<BTreeMap<api::PodId, PodRecord>, Report<PodServiceError>> {
    let records = state.active().await.map_err(state_error)?;
    let archived = state.archived().await.map_err(state_error)?;
    let stored = {
        let storage = Arc::clone(storage);
        blocking("list pod storage", move || storage.list_pods()).await?
    };
    let stored_ids = stored
        .iter()
        .map(|storage| storage.id().as_str().to_owned())
        .collect::<BTreeSet<_>>();
    let recorded_ids = records
        .iter()
        .map(|record| record.pod.id.0.to_string())
        .chain(archived.iter().map(|record| record.pod.id.0.to_string()))
        .collect::<BTreeSet<_>>();
    let root_ids = {
        let roots = Arc::clone(nix_roots);
        blocking("list pod Nix GC roots", move || roots.list()).await?
    };
    if let Some(orphan) = stored_ids.difference(&recorded_ids).next() {
        return Err(internal(format!(
            "pod storage {orphan} has no durable state"
        )));
    }

    for record in archived {
        cleanup_recovered_pod(&record, storage, runc, network, nix_roots, &stored_ids).await?;
    }

    let mut recovered = BTreeMap::new();
    for mut record in records {
        let runtime_id = runtime_id(&record.pod.id)?;
        cleanup_recovered_runtime(&record, runc, network).await?;

        if record.ephemeral {
            cleanup_recovered_files(&record, storage, nix_roots, &stored_ids).await?;
            state
                .archive(record.pod.id.clone(), Timestamp::now())
                .await
                .map_err(state_error)?;
            continue;
        }
        if record.persistent_state == PersistentPodState::Destroying {
            cleanup_recovered_files(&record, storage, nix_roots, &stored_ids).await?;
            state
                .archive(record.pod.id.clone(), Timestamp::now())
                .await
                .map_err(state_error)?;
            continue;
        }
        if stored_ids.contains(runtime_id.as_str()) {
            let pod_storage = stored
                .iter()
                .find(|storage| storage.id() == &runtime_id)
                .expect("stored pod identifiers were collected from these entries");
            if pod_storage.image() != &record.image {
                return Err(internal(format!(
                    "pod {} state pins a different image than its storage",
                    record.pod.id.0
                )));
            }
            if nix_enabled {
                let execution = runc
                    .execution(pod_storage, record.slot)
                    .map_err(|error| internal(format!("recover pod identity: {error}")))?;
                let roots = Arc::clone(nix_roots);
                let provisioned_id = runtime_id.clone();
                blocking("recover pod Nix GC roots", move || {
                    roots.provision(&provisioned_id, execution.uid, execution.gid)
                })
                .await?;
            } else {
                let roots = Arc::clone(nix_roots);
                let withdrawn_id = runtime_id.clone();
                blocking("withdraw disabled pod Nix GC roots", move || {
                    roots.withdraw(&withdrawn_id)
                })
                .await?;
            }
            if !matches!(record.pod.status, api::PodState::Failed(_)) {
                record.persistent_state = PersistentPodState::Ready;
                record.pod.status = api::PodState::Stopped;
            }
        } else {
            let roots = Arc::clone(nix_roots);
            let withdrawn_id = runtime_id.clone();
            blocking("withdraw roots for pod without storage", move || {
                roots.withdraw(&withdrawn_id)
            })
            .await?;
            record.pod.status = api::PodState::Failed(api::PodFailure {
                message: "pod storage is missing".into(),
                failed_at: Timestamp::now(),
            });
        }
        state.save(record.clone()).await.map_err(state_error)?;
        recovered.insert(record.pod.id.clone(), record);
    }
    let active_ids = recovered
        .keys()
        .map(|pod_id| pod_id.0.to_string())
        .collect::<BTreeSet<_>>();
    for orphan in root_ids
        .iter()
        .filter(|pod_id| !active_ids.contains(pod_id.as_str()))
    {
        let roots = Arc::clone(nix_roots);
        let orphan = orphan.clone();
        blocking("withdraw orphaned pod Nix GC roots", move || {
            roots.withdraw(&orphan)
        })
        .await?;
    }
    Ok(recovered)
}

/// Removes all runtime and persistent files left for an archived pod.
async fn cleanup_recovered_pod(
    record: &PodRecord,
    storage: &Arc<BtrfsStore>,
    runc: &Arc<Runc>,
    network: &NetworkManager,
    nix_roots: &Arc<NixRoots>,
    stored_ids: &BTreeSet<String>,
) -> Result<(), Report<PodServiceError>> {
    cleanup_recovered_runtime(record, runc, network).await?;
    cleanup_recovered_files(record, storage, nix_roots, stored_ids).await
}

/// Removes transient runtime resources found during startup recovery.
async fn cleanup_recovered_runtime(
    record: &PodRecord,
    runc: &Arc<Runc>,
    network: &NetworkManager,
) -> Result<(), Report<PodServiceError>> {
    let runtime_id = runtime_id(&record.pod.id)?;
    if runc.has_local_state(&runtime_id) {
        let runc = Arc::clone(runc);
        let stop_id = runtime_id.clone();
        let slot = record.slot;
        blocking("remove recovered runc state", move || {
            runc.stop(&stop_id, slot)
        })
        .await?;
    }
    let pod_network = PodNetwork::for_slot(record.slot)
        .map_err(|error| internal(format!("recover pod network: {error}")))?;
    network
        .remove(&pod_network)
        .await
        .map_err(|error| internal(format!("remove recovered pod network: {error}")))
}

/// Removes persistent pod files after destruction or archival.
async fn cleanup_recovered_files(
    record: &PodRecord,
    storage: &Arc<BtrfsStore>,
    nix_roots: &Arc<NixRoots>,
    stored_ids: &BTreeSet<String>,
) -> Result<(), Report<PodServiceError>> {
    let runtime_id = runtime_id(&record.pod.id)?;
    let roots = Arc::clone(nix_roots);
    let withdrawn_id = runtime_id.clone();
    blocking("withdraw recovered pod Nix GC roots", move || {
        roots.withdraw(&withdrawn_id)
    })
    .await?;
    if stored_ids.contains(runtime_id.as_str()) {
        let storage = Arc::clone(storage);
        blocking("remove recovered pod storage", move || {
            match storage.destroy_pod(&runtime_id) {
                Ok(()) | Err(StoreError::PodNotFound(_)) => Ok(()),
                Err(error) => Err(error),
            }
        })
        .await?;
    }
    Ok(())
}

/// Builds the trusted network principal from feature-owned pod state.
fn network_principal(
    record: &PodRecord,
    execution: &PodExecution,
    created_at_unix_ms: u64,
) -> NetworkPrincipal {
    NetworkPrincipal {
        id: NetworkPodId(record.pod.id.0.to_string()),
        name: record.pod.title.to_string(),
        title: Some(record.pod.title.to_string()),
        user: execution.user.clone(),
        uid: execution.uid,
        gid: execution.gid,
        created_at_unix_ms,
        health: Health::healthy(),
    }
}

/// Converts an API pod identifier to the validated runtime identifier.
fn runtime_id(pod_id: &api::PodId) -> Result<RuntimePodId, Report<PodServiceError>> {
    RuntimePodId::new(pod_id.0.to_string())
        .map_err(|error| internal(format!("invalid pod identifier: {error}")))
}

/// Generates a readable title from a pod identifier.
fn generated_title(pod_id: &api::PodId) -> tascarrel_api::ArcStr {
    let suffix = pod_id.0.rsplit('_').next().unwrap_or(pod_id.0.as_ref());
    format!("Pod {}", &suffix[..suffix.len().min(8)]).into()
}

/// Validates a client-supplied pod title.
fn validate_title(title: &str) -> Result<(), Report<PodServiceError>> {
    const MAX_TITLE_BYTES: usize = 256;
    if title.trim() != title || title.is_empty() || title.len() > MAX_TITLE_BYTES {
        return Err(invalid(format!(
            "pod title must contain 1 to {MAX_TITLE_BYTES} bytes without surrounding whitespace"
        )));
    }
    if title.chars().any(char::is_control) {
        return Err(invalid("pod title must not contain control characters"));
    }
    Ok(())
}

/// Applies one mutation to the workspace pod list.
fn reduce_pod_list(list: &mut api::PodList, mutation: &api::PodListMutation) {
    match mutation {
        api::PodListMutation::Upsert(pod) => {
            if let Some(index) = list.pods.iter().position(|existing| existing.id == pod.id) {
                list.pods[index] = pod.clone();
            } else {
                list.pods.push(pod.clone());
            }
        }
        api::PodListMutation::Remove(pod_id) => {
            if let Some(index) = list.pods.iter().position(|pod| pod.id == *pod_id) {
                list.pods.remove(index);
            }
        }
    }
}

/// Converts an API store stamp to the in-memory representation.
fn runtime_stamp(
    stamp: &store_api::Stamp,
) -> Result<tascarrel_store::Stamp, Report<PodServiceError>> {
    let generation = stamp.generation.parse::<uuid::Uuid>().map_err(|error| {
        PodServiceError::InvalidRequest("pod-list cursor generation is invalid".into())
            .report()
            .message(error.to_string())
    })?;
    Ok(tascarrel_store::Stamp {
        generation,
        version: stamp.version,
    })
}

/// Converts a statically non-zero service default.
fn nonzero_default(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("service default is statically non-zero")
}

/// Runs a blocking pod operation and converts infrastructure failures.
async fn blocking<T, E, F>(
    operation: &'static str,
    function: F,
) -> Result<T, Report<PodServiceError>>
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

/// Creates a contract error report.
fn invalid(message: impl Into<String>) -> Report<PodServiceError> {
    PodServiceError::InvalidRequest(message.into()).report()
}

/// Creates an internal error report.
fn internal(message: impl Into<String>) -> Report<PodServiceError> {
    PodServiceError::Internal(message.into()).report()
}

/// Combines a startup failure with the tail of podd's bounded startup log.
fn startup_failure_detail(message: &str, log: &str) -> String {
    const HEADING: &str = "\npod startup log (tail):\n";
    let message = message
        .trim()
        .chars()
        .take(ERROR_DETAIL_LIMIT)
        .collect::<String>();
    let available = ERROR_DETAIL_LIMIT.saturating_sub(message.chars().count() + HEADING.len());
    if available == 0 {
        return message;
    }
    let tail = log
        .trim()
        .chars()
        .rev()
        .take(available)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<String>();
    format!("{message}{HEADING}{tail}")
}

/// Converts a persistent-state failure into the service's internal category.
fn state_error(error: Report<PodStateError>) -> Report<PodServiceError> {
    error.escalate(PodServiceError::Internal(
        "persistent pod state operation failed".to_owned(),
    ))
}

/// Acquires a non-poisoning view of feature state.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use nix::unistd::getgid;
    use nix::unistd::getuid;

    use super::*;

    /// Verifies `SO_PEERCRED` exposes the listener-namespace PID and mapped
    /// identity used to authenticate podd before reading its nonce.
    #[tokio::test]
    async fn readiness_accepts_the_exact_peer_and_handshake() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ready.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = UnixListener::from_std(listener).unwrap();
        let handshake = b"TSRD01aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let client = tokio::spawn(async move {
            let mut stream = UnixStream::connect(path).await.unwrap();
            stream.write_all(handshake).await.unwrap();
            let mut acknowledgment = [0_u8; 1];
            stream.read_exact(&mut acknowledgment).await.unwrap();
            acknowledgment
        });

        let mut stream = accept_podd_readiness(
            &listener,
            std::process::id(),
            getuid().as_raw(),
            getgid().as_raw(),
            handshake,
        )
        .await
        .unwrap();
        stream.write_all(&[POD_READY_ACK]).await.unwrap();

        assert_eq!(client.await.unwrap(), [POD_READY_ACK]);
    }

    /// Verifies an authenticated podd connection cannot satisfy a different
    /// startup attempt's nonce.
    #[tokio::test]
    async fn readiness_rejects_a_stale_attempt_handshake() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ready.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = UnixListener::from_std(listener).unwrap();
        let client = tokio::spawn(async move {
            let mut stream = UnixStream::connect(path).await.unwrap();
            stream
                .write_all(b"TSRD01bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
                .await
                .unwrap();
        });

        let error = accept_podd_readiness(
            &listener,
            std::process::id(),
            getuid().as_raw(),
            getgid().as_raw(),
            b"TSRD01aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .await
        .unwrap_err();
        client.await.unwrap();

        assert!(error.to_string().contains("did not match this attempt"));
    }
}
