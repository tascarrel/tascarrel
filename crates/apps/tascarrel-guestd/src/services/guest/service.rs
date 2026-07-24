//! Guest information and bounded resource metric collection.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::time::Duration;

use jiff::Timestamp;
use reportify::ErrorExt as _;
use reportify::Report;
use tascarrel_api::ArcStr;
use tascarrel_api::types::common::JsonValue;
use tascarrel_api::types::guest as api;
use thiserror::Error;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio::time::MissedTickBehavior;
use tracing::warn;

/// Collects stable guest information and a bounded history of resource metrics.
///
/// Clones share one sampler and metric history. The sampler starts after one
/// complete sampling interval so its first CPU utilization value has a prior
/// measurement. Dropping the last service clone stops collection and closes
/// outstanding subscriptions after their retained samples are consumed.
#[derive(Clone)]
pub struct GuestService {
    inner: Arc<GuestServiceInner>,
}

impl GuestService {
    /// Starts metric collection for one guest incarnation.
    ///
    /// # Errors
    ///
    /// Returns [`GuestServiceError::InvalidConfig`] for a relative state
    /// directory or an interval too short for CPU measurement. Returns
    /// [`GuestServiceError::SystemInformation`] when the configured state
    /// filesystem or required system information cannot be resolved. Returns
    /// [`GuestServiceError::RuntimeUnavailable`] outside a Tokio runtime.
    #[tracing::instrument(
        level = "debug",
        skip(config),
        fields(state_directory = %config.state_directory.display())
    )]
    pub fn start(
        guest_instance_id: api::GuestInstanceId,
        config: GuestServiceConfig,
    ) -> Result<Self, Report<GuestServiceError>> {
        config.validate()?;
        tokio::runtime::Handle::try_current().map_err(|error| {
            error
                .escalate(GuestServiceError::RuntimeUnavailable)
                .message("failed to start guest metrics collector")
        })?;
        let (source, information) = SysinfoSource::open(&config.state_directory)?;
        Ok(Self::start_with_source(
            config,
            guest_instance_id,
            information,
            source,
        ))
    }

    /// Returns stable information captured when the service started.
    #[must_use]
    pub fn information(&self) -> api::QueryGuestInformationOutput {
        self.inner.information.clone()
    }

    /// Opens a resumable subscription to retained and live metric samples.
    #[must_use]
    pub(crate) fn subscribe(
        &self,
        input: api::GuestMetricsSubscription,
    ) -> GuestMetricsSubscription {
        self.inner.metrics.subscribe(input.cursor)
    }

    /// Starts the collector from resolved information and a metric source.
    fn start_with_source<S>(
        config: GuestServiceConfig,
        guest_instance_id: api::GuestInstanceId,
        source_information: SourceInformation,
        source: S,
    ) -> Self
    where
        S: MetricsSource,
    {
        let GuestServiceConfig {
            sample_interval,
            history_capacity,
            metrics_batch_capacity,
            mut properties,
            ..
        } = config;
        properties.extend(source_information.properties);
        let information = api::QueryGuestInformationOutput {
            guest_instance_id: guest_instance_id.clone(),
            logical_processor_count: source_information.logical_processor_count,
            memory_total_bytes: source_information.memory_total_bytes,
            state_disk_total_bytes: source_information.state_disk_total_bytes,
            properties,
        };
        let metrics =
            MetricsBuffer::new(guest_instance_id, history_capacity, metrics_batch_capacity);
        let collector = tokio::spawn(run_collector(source, metrics.clone(), sample_interval));
        Self {
            inner: Arc::new(GuestServiceInner {
                information,
                metrics,
                collector,
            }),
        }
    }
}

/// Configuration for guest information and metric collection.
#[derive(Clone, Debug)]
pub struct GuestServiceConfig {
    /// Persistent guest state directory used to identify the state filesystem.
    pub state_directory: PathBuf,
    /// Time between resource metric samples.
    pub sample_interval: Duration,
    /// Maximum number of metric samples retained for resumption.
    pub history_capacity: NonZeroUsize,
    /// Maximum number of metric samples emitted in one event.
    pub metrics_batch_capacity: NonZeroUsize,
    /// Additional stable diagnostic and discovery properties.
    ///
    /// Properties discovered by the service take precedence when a key
    /// conflicts with a caller-provided property.
    pub properties: HashMap<ArcStr, JsonValue>,
}

impl GuestServiceConfig {
    /// Creates a configuration with two-second samples and five minutes of
    /// retained history.
    #[must_use]
    pub fn new(state_directory: impl Into<PathBuf>) -> Self {
        Self {
            state_directory: state_directory.into(),
            sample_interval: DEFAULT_SAMPLE_INTERVAL,
            history_capacity: DEFAULT_HISTORY_CAPACITY,
            metrics_batch_capacity: DEFAULT_METRICS_BATCH_CAPACITY,
            properties: HashMap::new(),
        }
    }

    /// Validates invariants required by the system sampler.
    fn validate(&self) -> Result<(), Report<GuestServiceError>> {
        if !self.state_directory.is_absolute() {
            return Err(invalid_config("guest state directory must be absolute")
                .field_display("state_directory", self.state_directory.display()));
        }
        if self.sample_interval < sysinfo::MINIMUM_CPU_UPDATE_INTERVAL {
            return Err(invalid_config(
                "sample interval is shorter than the minimum CPU update interval",
            )
            .field_debug("sample_interval", self.sample_interval)
            .field_debug(
                "minimum_cpu_update_interval",
                sysinfo::MINIMUM_CPU_UPDATE_INTERVAL,
            ));
        }
        Ok(())
    }
}

/// Caller-relevant failure categories for starting the guest service.
#[derive(Debug, Error)]
pub enum GuestServiceError {
    /// The service configuration violates a required invariant.
    #[error("guest service configuration is invalid")]
    InvalidConfig,
    /// Required operating-system or filesystem information is unavailable.
    #[error("guest system information is unavailable")]
    SystemInformation,
    /// The asynchronous collector cannot be started.
    #[error("guest metrics collector runtime is unavailable")]
    RuntimeUnavailable,
}

/// Default interval between resource samples.
const DEFAULT_SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
/// Default number of samples retained for cursor resumption.
const DEFAULT_HISTORY_CAPACITY: NonZeroUsize =
    NonZeroUsize::new(150).expect("the default history capacity is non-zero");
/// Default number of samples emitted in one metric event.
const DEFAULT_METRICS_BATCH_CAPACITY: NonZeroUsize =
    NonZeroUsize::new(32).expect("the default metrics batch capacity is non-zero");

/// Resumable stream over the bounded guest metric history.
pub(crate) struct GuestMetricsSubscription {
    core: Arc<MetricsCore>,
    changed: watch::Receiver<u64>,
    cursor: Option<api::GuestMetricsCursor>,
}

impl GuestMetricsSubscription {
    /// Receives the next non-empty batch of retained or live metric samples.
    pub(crate) async fn recv(&mut self) -> Option<api::GuestMetricsEvent> {
        loop {
            {
                let state = lock_metrics_state(&self.core.state);
                if let Some(samples) = state.samples_after(
                    &self.core.guest_instance_id,
                    self.cursor.as_ref(),
                    self.core.batch_capacity,
                ) {
                    self.cursor = Some(
                        samples
                            .last()
                            .expect("a guest metrics event contains at least one sample")
                            .cursor
                            .clone(),
                    );
                    return Some(api::GuestMetricsEvent {
                        samples: samples.into(),
                    });
                }
                if state.closed {
                    return None;
                }
            }
            self.changed
                .changed()
                .await
                .expect("a metric subscription retains the watch sender");
        }
    }
}

/// Shared service state and ownership of the background collector.
struct GuestServiceInner {
    information: api::QueryGuestInformationOutput,
    metrics: MetricsBuffer,
    collector: JoinHandle<()>,
}

impl Drop for GuestServiceInner {
    fn drop(&mut self) {
        self.collector.abort();
        self.metrics.close();
    }
}

/// Supplies recurring metric measurements to the collector.
trait MetricsSource: Send + 'static {
    /// Captures one complete set of resource measurements.
    fn collect(&mut self) -> Result<MetricsValues, Report<GuestServiceError>>;
}

/// Stable capacities and properties discovered from a metric source.
struct SourceInformation {
    logical_processor_count: u32,
    memory_total_bytes: u64,
    state_disk_total_bytes: u64,
    properties: HashMap<ArcStr, JsonValue>,
}

/// Resource values captured during one sampling pass.
struct MetricsValues {
    observed_at: Timestamp,
    uptime_seconds: u64,
    cpu_usage_percent: f32,
    load_average: sysinfo::LoadAvg,
    memory_available_bytes: u64,
    swap_total_bytes: u64,
    swap_free_bytes: u64,
    state_disk_available_bytes: u64,
}

/// Long-lived `sysinfo` state scoped to the guest and its state filesystem.
struct SysinfoSource {
    system: sysinfo::System,
    disks: sysinfo::Disks,
    state_disk_index: usize,
}

impl SysinfoSource {
    /// Resolves stable information and initializes the recurring sampler.
    fn open(
        state_directory: &Path,
    ) -> Result<(Self, SourceInformation), Report<GuestServiceError>> {
        if !sysinfo::IS_SUPPORTED_SYSTEM {
            return Err(system_information(
                "the guest operating system is not supported by sysinfo",
            ));
        }
        let state_directory = std::fs::canonicalize(state_directory).map_err(|error| {
            error
                .escalate(GuestServiceError::SystemInformation)
                .message("failed to resolve guest state directory")
                .field_display("state_directory", state_directory.display())
        })?;
        let refreshes = sysinfo::RefreshKind::nothing()
            .with_cpu(sysinfo::CpuRefreshKind::nothing().with_cpu_usage())
            .with_memory(sysinfo::MemoryRefreshKind::everything());
        let system = sysinfo::System::new_with_specifics(refreshes);
        let logical_processor_count = u32::try_from(system.cpus().len()).map_err(|error| {
            error
                .escalate(GuestServiceError::SystemInformation)
                .message("logical processor count exceeds the API range")
        })?;
        if logical_processor_count == 0 {
            return Err(system_information(
                "sysinfo did not report any logical processors",
            ));
        }
        let memory_total_bytes = system.total_memory();
        if memory_total_bytes == 0 {
            return Err(system_information(
                "sysinfo did not report physical memory capacity",
            ));
        }

        let disk_refreshes = sysinfo::DiskRefreshKind::nothing().with_storage();
        let disks = sysinfo::Disks::new_with_refreshed_list_specifics(disk_refreshes);
        let state_disk_index = state_disk_index(&disks, &state_directory).ok_or_else(|| {
            system_information("could not identify the persistent state filesystem")
                .field_display("state_directory", state_directory.display())
        })?;
        let state_disk = &disks.list()[state_disk_index];
        let state_disk_total_bytes = state_disk.total_space();
        if state_disk_total_bytes == 0 {
            return Err(system_information(
                "sysinfo did not report persistent state filesystem capacity",
            )
            .field_display("mount_point", state_disk.mount_point().display()));
        }

        let mut properties = HashMap::new();
        insert_path_property(&mut properties, "paths.state_directory", &state_directory);
        insert_path_property(
            &mut properties,
            "paths.state_disk_mount",
            state_disk.mount_point(),
        );
        insert_string_property(
            &mut properties,
            "versions.guestd",
            env!("TASCARREL_BUILD_REVISION"),
        );
        insert_optional_string_property(
            &mut properties,
            "versions.kernel",
            sysinfo::System::kernel_version(),
        );
        insert_optional_string_property(
            &mut properties,
            "versions.operating_system",
            sysinfo::System::long_os_version(),
        );
        insert_string_property(
            &mut properties,
            "system.architecture",
            sysinfo::System::cpu_arch(),
        );

        Ok((
            Self {
                system,
                disks,
                state_disk_index,
            },
            SourceInformation {
                logical_processor_count,
                memory_total_bytes,
                state_disk_total_bytes,
                properties,
            },
        ))
    }
}

impl MetricsSource for SysinfoSource {
    fn collect(&mut self) -> Result<MetricsValues, Report<GuestServiceError>> {
        self.system.refresh_cpu_usage();
        self.system.refresh_memory();
        let state_disk = self
            .disks
            .list_mut()
            .get_mut(self.state_disk_index)
            .expect("the state disk index was resolved from this disk list");
        if !state_disk.refresh_specifics(sysinfo::DiskRefreshKind::nothing().with_storage()) {
            return Err(
                system_information("could not refresh the persistent state filesystem")
                    .field_display("mount_point", state_disk.mount_point().display()),
            );
        }
        Ok(MetricsValues {
            observed_at: Timestamp::now(),
            uptime_seconds: sysinfo::System::uptime(),
            cpu_usage_percent: self.system.global_cpu_usage(),
            load_average: sysinfo::System::load_average(),
            memory_available_bytes: self.system.available_memory(),
            swap_total_bytes: self.system.total_swap(),
            swap_free_bytes: self.system.free_swap(),
            state_disk_available_bytes: state_disk.available_space(),
        })
    }
}

/// Shared bounded storage for retained metric samples.
#[derive(Clone)]
struct MetricsBuffer {
    core: Arc<MetricsCore>,
}

impl MetricsBuffer {
    /// Creates an empty history with bounded retention and event batches.
    fn new(
        guest_instance_id: api::GuestInstanceId,
        capacity: NonZeroUsize,
        batch_capacity: NonZeroUsize,
    ) -> Self {
        let (changed, _) = watch::channel(0);
        Self {
            core: Arc::new(MetricsCore {
                guest_instance_id,
                capacity: capacity.get(),
                batch_capacity: batch_capacity.get(),
                state: Mutex::new(MetricsState {
                    last_position: 0,
                    samples: VecDeque::with_capacity(capacity.get()),
                    closed: false,
                }),
                changed,
            }),
        }
    }

    /// Appends one sample and discards the oldest sample at capacity.
    fn append(&self, values: &MetricsValues) {
        let mut state = lock_metrics_state(&self.core.state);
        if state.closed {
            return;
        }
        let position = state
            .last_position
            .checked_add(1)
            .expect("a VM incarnation cannot exhaust metric positions");
        state.samples.push_back(api::GuestMetricsSample {
            cursor: api::GuestMetricsCursor {
                guest_instance_id: self.core.guest_instance_id.clone(),
                position,
            },
            observed_at: values.observed_at,
            uptime_seconds: values.uptime_seconds,
            cpu: api::CpuMetrics {
                usage_percent: values.cpu_usage_percent,
                load_average: api::LoadAverage {
                    one_minute: values.load_average.one,
                    five_minutes: values.load_average.five,
                    fifteen_minutes: values.load_average.fifteen,
                },
            },
            memory: api::MemoryMetrics {
                available_bytes: values.memory_available_bytes,
                swap_total_bytes: values.swap_total_bytes,
                swap_free_bytes: values.swap_free_bytes,
            },
            state_disk: api::DiskMetrics {
                available_bytes: values.state_disk_available_bytes,
            },
        });
        if state.samples.len() > self.core.capacity {
            state.samples.pop_front();
        }
        state.last_position = position;
        self.core.changed.send_replace(position);
    }

    /// Opens a cursor-based view over retained and future samples.
    fn subscribe(&self, cursor: Option<api::GuestMetricsCursor>) -> GuestMetricsSubscription {
        GuestMetricsSubscription {
            core: Arc::clone(&self.core),
            changed: self.core.changed.subscribe(),
            cursor,
        }
    }

    /// Prevents further appends and wakes waiting subscribers.
    fn close(&self) {
        let mut state = lock_metrics_state(&self.core.state);
        if state.closed {
            return;
        }
        state.closed = true;
        self.core.changed.send_replace(state.last_position);
    }
}

/// State shared by the history and all of its subscriptions.
struct MetricsCore {
    guest_instance_id: api::GuestInstanceId,
    capacity: usize,
    batch_capacity: usize,
    state: Mutex<MetricsState>,
    changed: watch::Sender<u64>,
}

/// Mutable contents of one bounded metric history.
struct MetricsState {
    last_position: u64,
    samples: VecDeque<api::GuestMetricsSample>,
    closed: bool,
}

impl MetricsState {
    /// Selects the next batch of retained samples or rebases an invalid cursor.
    fn samples_after(
        &self,
        guest_instance_id: &api::GuestInstanceId,
        cursor: Option<&api::GuestMetricsCursor>,
        batch_capacity: usize,
    ) -> Option<Vec<api::GuestMetricsSample>> {
        let first_position = self.samples.front()?.cursor.position;
        let requested = cursor
            .filter(|cursor| {
                cursor.guest_instance_id == *guest_instance_id
                    && cursor.position <= self.last_position
            })
            .map_or(first_position, |cursor| {
                cursor.position.saturating_add(1).max(first_position)
            });
        if requested > self.last_position {
            return None;
        }
        let index = usize::try_from(requested - first_position)
            .expect("a retained metric offset fits in usize");
        Some(
            self.samples
                .iter()
                .skip(index)
                .take(batch_capacity)
                .cloned()
                .collect(),
        )
    }
}

/// Samples metrics until the owning service cancels this task.
async fn run_collector<S>(mut source: S, metrics: MetricsBuffer, sample_interval: Duration)
where
    S: MetricsSource,
{
    let mut interval = tokio::time::interval_at(Instant::now() + sample_interval, sample_interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        match source.collect() {
            Ok(values) => metrics.append(&values),
            Err(error) => warn!(error = ?error, "could not collect guest resource metrics"),
        }
    }
}

/// Finds the most specific filesystem mount containing the state directory.
fn state_disk_index(disks: &sysinfo::Disks, state_directory: &Path) -> Option<usize> {
    disks
        .list()
        .iter()
        .enumerate()
        .filter(|(_, disk)| state_directory.starts_with(disk.mount_point()))
        .max_by_key(|(_, disk)| disk.mount_point().components().count())
        .map(|(index, _)| index)
}

/// Inserts a path property when it has a lossless JSON string representation.
fn insert_path_property(properties: &mut HashMap<ArcStr, JsonValue>, key: &str, value: &Path) {
    if let Some(value) = value.to_str() {
        insert_string_property(properties, key, value);
    }
}

/// Inserts an available optional string property.
fn insert_optional_string_property(
    properties: &mut HashMap<ArcStr, JsonValue>,
    key: &str,
    value: Option<String>,
) {
    if let Some(value) = value {
        insert_string_property(properties, key, value);
    }
}

/// Serializes and inserts a string property.
fn insert_string_property(
    properties: &mut HashMap<ArcStr, JsonValue>,
    key: &str,
    value: impl Into<String>,
) {
    let value = serde_json::to_value(value.into())
        .expect("serializing a String into a JSON value cannot fail");
    properties.insert(key.into(), value);
}

/// Creates a configuration error report with contextual detail.
fn invalid_config(message: impl Into<String>) -> Report<GuestServiceError> {
    GuestServiceError::InvalidConfig
        .report()
        .message(message.into())
}

/// Creates a system-information error report with contextual detail.
fn system_information(message: impl Into<String>) -> Report<GuestServiceError> {
    GuestServiceError::SystemInformation
        .report()
        .message(message.into())
}

/// Acquires shared metric state and reports recovery from lock poisoning.
fn lock_metrics_state<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            warn!("guest metric state lock was poisoned; recovering retained state");
            poisoned.into_inner()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies new and resumed subscriptions replay every available sample.
    #[tokio::test]
    async fn metric_history_resumes_and_rebases() {
        let guest_instance_id = api::GuestInstanceId::generate();
        let metrics = MetricsBuffer::new(
            guest_instance_id.clone(),
            NonZeroUsize::new(2).expect("test capacity is non-zero"),
            NonZeroUsize::new(2).expect("test batch capacity is non-zero"),
        );
        metrics.append(&test_values(1));
        metrics.append(&test_values(2));
        metrics.append(&test_values(3));
        metrics.close();

        let mut retained = metrics.subscribe(Some(api::GuestMetricsCursor {
            guest_instance_id: guest_instance_id.clone(),
            position: 1,
        }));
        assert_eq!(next_positions(&mut retained).await, [2, 3]);
        assert!(retained.recv().await.is_none());

        let mut expired = metrics.subscribe(Some(api::GuestMetricsCursor {
            guest_instance_id: guest_instance_id.clone(),
            position: 0,
        }));
        assert_eq!(next_positions(&mut expired).await, [2, 3]);

        let mut prior_instance = metrics.subscribe(Some(api::GuestMetricsCursor {
            guest_instance_id: api::GuestInstanceId::generate(),
            position: 3,
        }));
        assert_eq!(next_positions(&mut prior_instance).await, [2, 3]);

        let mut current = metrics.subscribe(None);
        assert_eq!(next_positions(&mut current).await, [2, 3]);
    }

    /// Verifies a retained metric backlog is split at the event capacity.
    #[tokio::test]
    async fn metric_history_limits_each_sample_batch() {
        let guest_instance_id = api::GuestInstanceId::generate();
        let metrics = MetricsBuffer::new(
            guest_instance_id.clone(),
            NonZeroUsize::new(3).expect("test capacity is non-zero"),
            NonZeroUsize::new(2).expect("test batch capacity is non-zero"),
        );
        metrics.append(&test_values(1));
        metrics.append(&test_values(2));
        metrics.append(&test_values(3));
        metrics.close();
        let mut subscription = metrics.subscribe(Some(api::GuestMetricsCursor {
            guest_instance_id,
            position: 0,
        }));

        assert_eq!(next_positions(&mut subscription).await, [1, 2]);
        assert_eq!(next_positions(&mut subscription).await, [3]);
        assert!(subscription.recv().await.is_none());
    }

    /// Verifies the service publishes static information and samples from one
    /// shared source.
    #[tokio::test]
    async fn service_exposes_information_and_collected_metrics() {
        let guest_instance_id = api::GuestInstanceId::generate();
        let mut config = GuestServiceConfig::new("/fixture/state");
        config.sample_interval = Duration::from_millis(1);
        config.history_capacity = NonZeroUsize::new(2).expect("test capacity is non-zero");
        insert_string_property(
            &mut config.properties,
            "paths.runtime_directory",
            "/run/tascarrel",
        );
        let mut source_properties = HashMap::new();
        insert_string_property(&mut source_properties, "versions.kernel", "fixture-kernel");
        let service = GuestService::start_with_source(
            config,
            guest_instance_id.clone(),
            SourceInformation {
                logical_processor_count: 4,
                memory_total_bytes: 8 * 1024,
                state_disk_total_bytes: 64 * 1024,
                properties: source_properties,
            },
            FakeSource { next: 0 },
        );

        let information = service.information();
        assert_eq!(information.guest_instance_id, guest_instance_id);
        assert_eq!(information.logical_processor_count, 4);
        assert_eq!(information.memory_total_bytes, 8 * 1024);
        assert_eq!(information.state_disk_total_bytes, 64 * 1024);
        assert_eq!(
            information.properties.get("versions.kernel"),
            Some(&serde_json::to_value("fixture-kernel").expect("fixture serializes"))
        );
        assert!(
            information
                .properties
                .contains_key("paths.runtime_directory")
        );

        let mut subscription = service.subscribe(api::GuestMetricsSubscription { cursor: None });
        let event = tokio::time::timeout(Duration::from_secs(1), subscription.recv())
            .await
            .expect("collector produces a sample before the timeout")
            .expect("collector remains open");
        let [sample] = event.samples.as_ref() else {
            panic!("the first collection produces one sample");
        };
        assert_eq!(sample.cursor.position, 1);
        assert_eq!(sample.uptime_seconds, 1);
        assert_eq!(sample.memory.available_bytes, 1023);
        assert_eq!(sample.state_disk.available_bytes, 4095);
    }

    struct FakeSource {
        next: u16,
    }

    impl MetricsSource for FakeSource {
        fn collect(&mut self) -> Result<MetricsValues, Report<GuestServiceError>> {
            self.next += 1;
            Ok(test_values(self.next))
        }
    }

    async fn next_positions(subscription: &mut GuestMetricsSubscription) -> Vec<u64> {
        subscription
            .recv()
            .await
            .expect("metric subscription remains open")
            .samples
            .iter()
            .map(|sample| sample.cursor.position)
            .collect()
    }

    fn test_values(marker: u16) -> MetricsValues {
        let marker_bytes = u64::from(marker);
        MetricsValues {
            observed_at: "2026-07-19T18:15:00Z"
                .parse()
                .expect("fixture timestamp is valid"),
            uptime_seconds: marker_bytes,
            cpu_usage_percent: f32::from(marker),
            load_average: sysinfo::LoadAvg {
                one: f64::from(marker),
                five: f64::from(marker),
                fifteen: f64::from(marker),
            },
            memory_available_bytes: 1024 - marker_bytes,
            swap_total_bytes: 512,
            swap_free_bytes: 512 - marker_bytes,
            state_disk_available_bytes: 4096 - marker_bytes,
        }
    }
}
