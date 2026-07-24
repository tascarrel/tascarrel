//! Resumable synchronization for mutation-driven state.
//!
//! A [`Store`] owns authoritative state and a bounded journal of mutations.
//! Subscribers identify their local state with a [`Stamp`]. The store replays
//! retained mutations when possible and sends a complete [`Snapshot`] when a
//! subscriber has no state or can no longer resume from its stamp.
//!
//! A [`Cache`] is the corresponding consumer-side state holder. It applies the
//! same reducer to streamed mutations and exposes its current stamp for the
//! next subscription attempt.
//!
//! State and mutations are shared through [`Arc`]. Applying a mutation uses
//! [`Arc::make_mut`], so an outstanding snapshot remains immutable and state is
//! cloned only when the current value is shared.
//!
//! The crate does not define a wire format. API layers should map stamps,
//! snapshots, and mutations to Sidex-defined transport types.
//!
//! ```
//! use std::num::NonZeroUsize;
//! use tascarrel_store::Cache;
//! use tascarrel_store::Store;
//!
//! # async fn example() -> tascarrel_store::CacheResult<()> {
//! let reduce = |value: &mut u64, mutation: &u64| *value += mutation;
//! let store = Store::new(0, reduce, NonZeroUsize::new(128).unwrap());
//! let mut cache = Cache::new(reduce);
//!
//! let mut subscription = store.subscribe(cache.stamp());
//! let initial = subscription.recv().await.unwrap();
//! cache.apply(initial)?;
//!
//! store.apply(3);
//! cache.apply(subscription.recv().await.unwrap())?;
//! assert_eq!(cache.value(), Some(&3));
//! # Ok(())
//! # }
//! ```

#![deny(unsafe_code)]

use std::collections::VecDeque;
use std::future::poll_fn;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::Weak;
use std::task::Context;
use std::task::Poll;

use futures_core::Stream;
use futures_util::StreamExt as _;
use futures_util::stream;
use reportify::ErrorExt as _;
use reportify::Report;
use thiserror::Error as ThisError;
use tokio::sync::watch;
use uuid::Uuid;

/// Authoritative state with a bounded mutation journal.
pub struct Store<T, M> {
    core: Arc<StoreCore<T, M>>,
}

impl<T, M> Store<T, M>
where
    T: Clone + Send + Sync + 'static,
    M: Send + Sync + 'static,
{
    /// Creates a store at version zero with a fresh `UUIDv4` generation.
    ///
    /// The `history_limit` determines how far subscribers can catch up without
    /// receiving a complete snapshot.
    pub fn new<R>(initial_value: T, reducer: R, history_limit: NonZeroUsize) -> Self
    where
        R: Reduce<T, M>,
    {
        let generation = Uuid::new_v4();
        let stamp = Stamp {
            generation,
            version: 0,
        };
        let (changes, _) = watch::channel(stamp);
        Self {
            core: Arc::new(StoreCore {
                reducer: Arc::new(reducer),
                state: Mutex::new(StoreState {
                    generation,
                    version: 0,
                    value: Arc::new(initial_value),
                    history: VecDeque::new(),
                    history_limit,
                }),
                changes,
            }),
        }
    }

    /// Applies a mutation and returns the stamp of the resulting state.
    ///
    /// A version rollover starts a fresh generation. Existing subscribers
    /// then receive a snapshot before observing further mutations.
    pub fn apply(&self, mutation: M) -> Stamp {
        let mutation = Arc::new(mutation);
        let mut state = lock(&self.core.state);
        self.core
            .reducer
            .reduce(Arc::make_mut(&mut state.value), mutation.as_ref());

        if state.version == u64::MAX {
            state.generation = Uuid::new_v4();
            state.version = 1;
            state.history.clear();
        } else {
            state.version += 1;
        }

        state.history.push_back(mutation);
        if state.history.len() > state.history_limit.get() {
            state.history.pop_front();
        }

        let stamp = state.stamp();
        self.core.changes.send_replace(stamp);
        stamp
    }

    /// Returns a constant-time snapshot of the current state.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot<T> {
        lock(&self.core.state).snapshot()
    }

    /// Subscribes after a state the consumer already holds.
    ///
    /// With no stamp, a different generation, a future version, or a version
    /// older than the retained journal, the first event is a snapshot. A
    /// resumable stamp receives only later mutations. If a live subscriber
    /// falls behind the journal, it is automatically rebased with a snapshot.
    #[must_use]
    pub fn subscribe(&self, after: Option<Stamp>) -> Subscription<T, M> {
        let changes = self.core.changes.subscribe();
        let mut pending = VecDeque::new();
        lock(&self.core.state).collect_events_after(after, &mut pending);
        Subscription::new(SubscriptionState {
            store: Arc::downgrade(&self.core),
            changes,
            cursor: after,
            pending,
        })
    }
}

impl<T, M> Clone for Store<T, M> {
    fn clone(&self) -> Self {
        Self {
            core: Arc::clone(&self.core),
        }
    }
}

/// Single-owner consumer-side state maintained from [`StoreEvent`]s.
///
/// A cache is intentionally not clonable. Feed it events from one ordered
/// subscription at a time through [`Cache::apply`].
pub struct Cache<T, M> {
    reducer: Box<dyn Reduce<T, M>>,
    snapshot: Option<Snapshot<T>>,
}

impl<T, M> Cache<T, M>
where
    T: Clone + 'static,
    M: 'static,
{
    /// Creates an empty cache.
    pub fn new<R>(reducer: R) -> Self
    where
        R: Reduce<T, M>,
    {
        Self {
            reducer: Box::new(reducer),
            snapshot: None,
        }
    }

    /// Creates a cache from a previously persisted snapshot.
    pub fn from_snapshot<R>(snapshot: Snapshot<T>, reducer: R) -> Self
    where
        R: Reduce<T, M>,
    {
        Self {
            reducer: Box::new(reducer),
            snapshot: Some(snapshot),
        }
    }

    /// Returns the stamp to use when resuming a subscription.
    #[must_use]
    pub fn stamp(&self) -> Option<Stamp> {
        self.snapshot.as_ref().map(|snapshot| snapshot.stamp)
    }

    /// Returns the current cached value.
    #[must_use]
    pub fn value(&self) -> Option<&T> {
        self.snapshot
            .as_ref()
            .map(|snapshot| snapshot.value.as_ref())
    }

    /// Returns the current cached snapshot.
    #[must_use]
    pub fn snapshot(&self) -> Option<Snapshot<T>> {
        self.snapshot.clone()
    }

    /// Applies one snapshot or mutation from a subscription.
    ///
    /// A snapshot from another generation replaces the cached state. A newer
    /// snapshot from the current generation advances it, while duplicate or
    /// older snapshots and mutations are ignored. A mutation without a base
    /// snapshot, from another generation, or with a version gap is rejected.
    ///
    /// # Errors
    ///
    /// Returns [`CacheError::MissingSnapshot`],
    /// [`CacheError::GenerationMismatch`], or [`CacheError::VersionGap`] when
    /// the event cannot be applied safely.
    pub fn apply(&mut self, event: StoreEvent<T, M>) -> CacheResult<()> {
        match event {
            StoreEvent::Snapshot(snapshot) => {
                if self.snapshot.as_ref().is_some_and(|cached| {
                    cached.stamp.generation == snapshot.stamp.generation
                        && cached.stamp.version >= snapshot.stamp.version
                }) {
                    return Ok(());
                }
                self.snapshot = Some(snapshot);
                Ok(())
            }
            StoreEvent::Mutation(mutation) => {
                let snapshot = self
                    .snapshot
                    .as_mut()
                    .ok_or_else(|| CacheError::MissingSnapshot.report())?;
                if snapshot.stamp.generation != mutation.stamp.generation {
                    return Err(CacheError::GenerationMismatch {
                        cached: snapshot.stamp.generation,
                        mutation: mutation.stamp.generation,
                    }
                    .report());
                }
                if mutation.stamp.version <= snapshot.stamp.version {
                    return Ok(());
                }
                let expected = snapshot.stamp.version.saturating_add(1);
                if mutation.stamp.version != expected {
                    return Err(CacheError::VersionGap {
                        cached: snapshot.stamp.version,
                        mutation: mutation.stamp.version,
                    }
                    .report());
                }

                self.reducer.reduce(
                    Arc::make_mut(&mut snapshot.value),
                    mutation.mutation.as_ref(),
                );
                snapshot.stamp = mutation.stamp;
                Ok(())
            }
        }
    }

    /// Applies events until an asynchronous stream ends.
    ///
    /// This mutably borrows the cache for the stream's lifetime. Consumers
    /// that inspect state between live events should call
    /// [`Subscription::recv`] and [`Cache::apply`] in their own event loop
    /// instead.
    ///
    /// # Errors
    ///
    /// Returns the first cache consistency error encountered while consuming
    /// the stream.
    pub async fn synchronize<S>(&mut self, events: S) -> CacheResult<()>
    where
        S: Stream<Item = StoreEvent<T, M>>,
    {
        let mut events = Box::pin(events);
        while let Some(event) = events.next().await {
            self.apply(event)?;
        }
        Ok(())
    }
}

/// Applies mutations to stored or cached state.
///
/// Implementations must not panic. A panic may leave copy-on-write state
/// partially mutated without advancing its stamp.
pub trait Reduce<T, M>: Send + Sync + 'static {
    /// Applies one mutation to a value.
    fn reduce(&self, value: &mut T, mutation: &M);
}

impl<T, M, F> Reduce<T, M> for F
where
    F: Fn(&mut T, &M) + Send + Sync + 'static,
{
    fn reduce(&self, value: &mut T, mutation: &M) {
        self(value, mutation);
    }
}

/// An asynchronous stream of snapshots and mutations.
pub struct Subscription<T, M> {
    events: Pin<Box<dyn Stream<Item = StoreEvent<T, M>> + Send + 'static>>,
}

impl<T, M> Subscription<T, M> {
    /// Waits for the next event, or returns `None` after the store is dropped.
    pub async fn recv(&mut self) -> Option<StoreEvent<T, M>> {
        poll_fn(|context| Pin::new(&mut *self).poll_next(context)).await
    }
}

impl<T, M> Stream for Subscription<T, M> {
    type Item = StoreEvent<T, M>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.events.as_mut().poll_next(context)
    }
}

impl<T, M> Subscription<T, M>
where
    T: Send + Sync + 'static,
    M: Send + Sync + 'static,
{
    /// Builds a stream that reconciles its cursor against the journal after
    /// every change notification, so coalesced notifications cannot lose data.
    fn new(state: SubscriptionState<T, M>) -> Self {
        let events = stream::unfold(state, |mut state| async move {
            loop {
                if let Some(event) = state.pending.pop_front() {
                    state.cursor = Some(event.stamp());
                    return Some((event, state));
                }

                state.changes.changed().await.ok()?;
                let store = state.store.upgrade()?;
                lock(&store.state).collect_events_after(state.cursor, &mut state.pending);
            }
        });
        Self {
            events: Box::pin(events),
        }
    }
}

/// Identifies one exact state in a store generation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Stamp {
    /// `UUIDv4` identifying the lifetime of the authoritative store state.
    pub generation: Uuid,
    /// Number of mutations applied within the generation.
    pub version: u64,
}

/// Complete state at a specific stamp.
#[derive(Debug, Eq, PartialEq)]
pub struct Snapshot<T> {
    /// Stamp identifying this state.
    pub stamp: Stamp,
    /// Immutable shared state value.
    pub value: Arc<T>,
}

impl<T> Clone for Snapshot<T> {
    fn clone(&self) -> Self {
        Self {
            stamp: self.stamp,
            value: Arc::clone(&self.value),
        }
    }
}

/// One mutation and the stamp of its resulting state.
#[derive(Debug, Eq, PartialEq)]
pub struct StampedMutation<M> {
    /// Stamp of the state after applying the mutation.
    pub stamp: Stamp,
    /// Immutable shared mutation value.
    pub mutation: Arc<M>,
}

impl<M> Clone for StampedMutation<M> {
    fn clone(&self) -> Self {
        Self {
            stamp: self.stamp,
            mutation: Arc::clone(&self.mutation),
        }
    }
}

/// An item emitted by a [`Subscription`].
#[derive(Debug, Eq, PartialEq)]
pub enum StoreEvent<T, M> {
    /// Replaces all consumer-side state.
    Snapshot(Snapshot<T>),
    /// Advances consumer-side state by one version.
    Mutation(StampedMutation<M>),
}

impl<T, M> StoreEvent<T, M> {
    /// Returns the state stamp produced by this event.
    #[must_use]
    pub const fn stamp(&self) -> Stamp {
        match self {
            Self::Snapshot(snapshot) => snapshot.stamp,
            Self::Mutation(mutation) => mutation.stamp,
        }
    }
}

impl<T, M> Clone for StoreEvent<T, M> {
    fn clone(&self) -> Self {
        match self {
            Self::Snapshot(snapshot) => Self::Snapshot(snapshot.clone()),
            Self::Mutation(mutation) => Self::Mutation(mutation.clone()),
        }
    }
}

/// A cache consistency error carried by [`Report`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, ThisError)]
pub enum CacheError {
    /// The cache must first receive a complete snapshot.
    #[error("a mutation cannot be applied before the cache has a snapshot")]
    MissingSnapshot,
    /// A mutation belongs to a different store generation.
    #[error("mutation generation {mutation} does not match cached generation {cached}")]
    GenerationMismatch {
        /// Generation currently held by the cache.
        cached: Uuid,
        /// Generation carried by the mutation.
        mutation: Uuid,
    },
    /// One or more mutations are missing before the received mutation.
    #[error("mutation version {mutation} does not immediately follow cached version {cached}")]
    VersionGap {
        /// Version currently held by the cache.
        cached: u64,
        /// Version carried by the mutation.
        mutation: u64,
    },
}

/// The result type returned by cache operations.
pub type CacheResult<T> = std::result::Result<T, Report<CacheError>>;

/// Shared authoritative state and its change notification channel.
struct StoreCore<T, M> {
    reducer: Arc<dyn Reduce<T, M>>,
    state: Mutex<StoreState<T, M>>,
    changes: watch::Sender<Stamp>,
}

/// State protected by the store's mutation lock.
struct StoreState<T, M> {
    generation: Uuid,
    version: u64,
    value: Arc<T>,
    history: VecDeque<Arc<M>>,
    history_limit: NonZeroUsize,
}

impl<T, M> StoreState<T, M> {
    /// Returns the stamp corresponding to `value`.
    fn stamp(&self) -> Stamp {
        Stamp {
            generation: self.generation,
            version: self.version,
        }
    }

    /// Shares the current value without copying it.
    fn snapshot(&self) -> Snapshot<T> {
        Snapshot {
            stamp: self.stamp(),
            value: Arc::clone(&self.value),
        }
    }

    /// Collects the shortest valid catch-up sequence after a consumer stamp.
    ///
    /// A snapshot replaces an unusable cursor; otherwise history positions
    /// derive their versions from the current version and history length. The
    /// existing buffer allocation is retained for subsequent reconciliations.
    fn collect_events_after(&self, after: Option<Stamp>, events: &mut VecDeque<StoreEvent<T, M>>) {
        events.clear();
        let Some(after) = after else {
            events.push_back(StoreEvent::Snapshot(self.snapshot()));
            return;
        };
        if after.generation != self.generation || after.version > self.version {
            events.push_back(StoreEvent::Snapshot(self.snapshot()));
            return;
        }

        let history_len = u64::try_from(self.history.len()).unwrap_or(self.version);
        let base_version = self.version.saturating_sub(history_len);
        if after.version < base_version {
            events.push_back(StoreEvent::Snapshot(self.snapshot()));
            return;
        }

        let history_offset =
            usize::try_from(after.version - base_version).unwrap_or(self.history.len());
        events.reserve(self.history.len().saturating_sub(history_offset));
        for (version, mutation) in (after.version.saturating_add(1)..=self.version)
            .zip(self.history.iter().skip(history_offset))
        {
            events.push_back(StoreEvent::Mutation(StampedMutation {
                stamp: Stamp {
                    generation: self.generation,
                    version,
                },
                mutation: Arc::clone(mutation),
            }));
        }
    }
}

/// Cursor, buffered catch-up events, and wake receiver for one subscription.
struct SubscriptionState<T, M> {
    store: Weak<StoreCore<T, M>>,
    changes: watch::Receiver<Stamp>,
    cursor: Option<Stamp>,
    pending: VecDeque<StoreEvent<T, M>>,
}

/// Acquires state while retaining access after a reducer panic poisons it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroUsize;
    use std::sync::Arc;

    use futures_util::stream as futures_stream;

    use super::*;

    fn retention(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).expect("test retention is nonzero")
    }

    #[allow(clippy::trivially_copy_pass_by_ref)]
    fn add(value: &mut u64, mutation: &u64) {
        *value += mutation;
    }

    /// Verifies retained replay and snapshot rebasing for stale stamps.
    #[tokio::test]
    async fn subscription_replays_retained_mutations_and_rebases_stale_state() {
        let store = Store::new(0, add, retention(2));
        let initial = store.snapshot();
        store.apply(1);
        store.apply(2);

        let mut replay = store.subscribe(Some(initial.stamp));
        let first = replay.recv().await.expect("first replayed mutation");
        let second = replay.recv().await.expect("second replayed mutation");
        assert!(matches!(first, StoreEvent::Mutation(ref event) if *event.mutation == 1));
        assert!(matches!(second, StoreEvent::Mutation(ref event) if *event.mutation == 2));
        assert_eq!(first.stamp().version, 1);
        assert_eq!(second.stamp().version, 2);

        store.apply(3);
        let mut stale = store.subscribe(Some(initial.stamp));
        let StoreEvent::Snapshot(snapshot) = stale.recv().await.expect("replacement snapshot")
        else {
            panic!("stale subscriber did not receive a snapshot");
        };
        assert_eq!(*snapshot.value, 6);
        assert_eq!(snapshot.stamp.version, 3);
    }

    /// Verifies that a subscriber falling behind receives a fresh snapshot.
    #[tokio::test]
    async fn slow_subscription_recovers_with_a_snapshot() {
        let store = Store::new(0, add, retention(2));
        let initial = store.snapshot();
        let mut subscription = store.subscribe(Some(initial.stamp));

        store.apply(1);
        store.apply(2);
        store.apply(3);

        let StoreEvent::Snapshot(snapshot) = subscription.recv().await.expect("recovery snapshot")
        else {
            panic!("slow subscriber did not receive a snapshot");
        };
        assert_eq!(*snapshot.value, 6);
        assert_eq!(snapshot.stamp.version, 3);
    }

    /// Verifies that outstanding snapshots retain their original state.
    #[tokio::test]
    async fn snapshots_are_copy_on_write() {
        let store = Store::new(4, add, retention(2));
        let before = store.snapshot();
        let same_version = store.snapshot();
        assert!(Arc::ptr_eq(&before.value, &same_version.value));

        store.apply(3);
        let after = store.snapshot();
        assert_eq!(*before.value, 4);
        assert_eq!(*after.value, 7);
        assert!(!Arc::ptr_eq(&before.value, &after.value));
    }

    /// Verifies cache resumption, copy-on-write state, and duplicate handling.
    #[tokio::test]
    async fn cache_resumes_and_ignores_duplicate_mutations() {
        let store = Store::new(0, add, retention(8));
        let mut cache = Cache::new(add);

        let mut initial = store.subscribe(cache.stamp());
        cache
            .apply(initial.recv().await.expect("initial snapshot"))
            .expect("apply initial snapshot");
        let held = cache.snapshot().expect("held cached snapshot");

        store.apply(5);
        store.apply(7);
        let mut resumed = store.subscribe(cache.stamp());
        let first = resumed.recv().await.expect("first resumed mutation");
        let second = resumed.recv().await.expect("second resumed mutation");
        cache.apply(first.clone()).expect("apply mutation");
        cache.apply(first).expect("ignore duplicate mutation");
        cache.apply(second).expect("apply second mutation");

        assert_eq!(*held.value, 0);
        assert_eq!(cache.value(), Some(&12));
        assert_eq!(cache.stamp().expect("cache stamp").version, 2);
    }

    /// Verifies finite stream consumption, stale snapshots, and gap rejection.
    #[tokio::test]
    async fn cache_synchronizes_finite_async_streams_and_rejects_gaps() {
        let generation = Uuid::new_v4();
        let snapshot = StoreEvent::Snapshot(Snapshot {
            stamp: Stamp {
                generation,
                version: 4,
            },
            value: Arc::new(10),
        });
        let mutation = StoreEvent::Mutation(StampedMutation {
            stamp: Stamp {
                generation,
                version: 5,
            },
            mutation: Arc::new(2),
        });
        let mut cache = Cache::new(add);
        cache
            .synchronize(futures_stream::iter([snapshot, mutation]))
            .await
            .expect("synchronize stream");
        assert_eq!(cache.value(), Some(&12));

        cache
            .apply(StoreEvent::Snapshot(Snapshot {
                stamp: Stamp {
                    generation,
                    version: 4,
                },
                value: Arc::new(99),
            }))
            .expect("ignore stale snapshot");
        assert_eq!(cache.value(), Some(&12));

        let gap = StoreEvent::Mutation(StampedMutation {
            stamp: Stamp {
                generation,
                version: 7,
            },
            mutation: Arc::new(1),
        });
        let report = cache.apply(gap).expect_err("reject version gap");
        assert_eq!(
            report.error(),
            &CacheError::VersionGap {
                cached: 5,
                mutation: 7,
            }
        );
        assert_eq!(cache.value(), Some(&12));
    }

    /// Verifies total version ordering across concurrent store mutations.
    #[tokio::test]
    async fn concurrent_mutations_have_one_total_order() {
        let store = Store::new(0, add, retention(32));
        let initial = store.snapshot();
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let store = store.clone();
            tasks.push(tokio::spawn(async move { store.apply(1) }));
        }
        let mut versions = Vec::new();
        for task in tasks {
            versions.push(task.await.expect("mutation task").version);
        }
        versions.sort_unstable();
        assert_eq!(versions, (1..=16).collect::<Vec<_>>());

        let mut replay = store.subscribe(Some(initial.stamp));
        for version in 1..=16 {
            let event = replay.recv().await.expect("ordered replay event");
            assert_eq!(event.stamp().version, version);
        }
    }

    /// Verifies that a current maximum version produces no catch-up events.
    #[test]
    fn current_max_version_has_no_catch_up_events() {
        let generation = Uuid::new_v4();
        let state = StoreState {
            generation,
            version: u64::MAX,
            value: Arc::new(1),
            history: VecDeque::from([Arc::new(1)]),
            history_limit: retention(1),
        };
        let mut events = VecDeque::new();

        state.collect_events_after(
            Some(Stamp {
                generation,
                version: u64::MAX,
            }),
            &mut events,
        );

        assert!(events.is_empty());
    }
}
