//! Bounded resumable activity streams owned by the host network service.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;

use tascarrel_api::types::host::HostInstanceId;
use tokio::sync::watch;

/// One contiguous non-empty batch and the position of its final entry.
#[derive(Debug)]
pub(crate) struct ActivityBatch<T> {
    pub(crate) position: u64,
    pub(crate) entries: Vec<T>,
}

/// Cloneable bounded stream shared by producers and subscriptions.
#[derive(Debug)]
pub(crate) struct ActivityStream<T> {
    inner: Arc<ActivityStreamInner<T>>,
}

impl<T> Clone for ActivityStream<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[derive(Debug)]
struct ActivityStreamInner<T> {
    host_instance_id: HostInstanceId,
    capacity: usize,
    batch_limit: usize,
    state: Mutex<ActivityState<T>>,
    changed: watch::Sender<u64>,
}

#[derive(Debug)]
struct ActivityState<T> {
    entries: VecDeque<(u64, T)>,
    next_position: u64,
}

/// Resumable reader for one activity stream.
#[derive(Debug)]
pub struct ActivitySubscription<T> {
    stream: ActivityStream<T>,
    next_position: u64,
    changed: watch::Receiver<u64>,
}

impl<T> ActivityStream<T> {
    pub(crate) fn new(
        host_instance_id: HostInstanceId,
        capacity: NonZeroUsize,
        batch_limit: NonZeroUsize,
    ) -> Self {
        let (changed, _) = watch::channel(0);
        Self {
            inner: Arc::new(ActivityStreamInner {
                host_instance_id,
                capacity: capacity.get(),
                batch_limit: batch_limit.get(),
                state: Mutex::new(ActivityState {
                    entries: VecDeque::with_capacity(capacity.get()),
                    next_position: 1,
                }),
                changed,
            }),
        }
    }

    pub(crate) fn append(&self, entry: T) -> u64 {
        let position = {
            let mut state = lock(&self.inner.state);
            let position = state.next_position;
            state.next_position = state
                .next_position
                .checked_add(1)
                .expect("network activity position does not exhaust u64");
            if state.entries.len() == self.inner.capacity {
                state.entries.pop_front();
            }
            state.entries.push_back((position, entry));
            position
        };
        self.inner.changed.send_replace(position);
        position
    }

    pub(crate) fn subscribe(
        &self,
        cursor: Option<(&HostInstanceId, u64)>,
    ) -> ActivitySubscription<T> {
        let next_position = {
            let state = lock(&self.inner.state);
            let oldest = state
                .entries
                .front()
                .map_or(state.next_position, |(position, _)| *position);
            let newest = state.next_position - 1;
            match cursor {
                Some((host, position))
                    if host == &self.inner.host_instance_id
                        && position >= oldest.saturating_sub(1)
                        && position <= newest =>
                {
                    position + 1
                }
                _ => oldest,
            }
        };
        ActivitySubscription {
            stream: self.clone(),
            next_position,
            changed: self.inner.changed.subscribe(),
        }
    }

    pub(crate) fn host_instance_id(&self) -> &HostInstanceId {
        &self.inner.host_instance_id
    }
}

impl<T: Clone> ActivitySubscription<T> {
    pub(crate) async fn recv(&mut self) -> Option<ActivityBatch<T>> {
        loop {
            let batch = {
                let state = lock(&self.stream.inner.state);
                let oldest = state
                    .entries
                    .front()
                    .map_or(state.next_position, |(position, _)| *position);
                if self.next_position < oldest {
                    self.next_position = oldest;
                }
                let entries = state
                    .entries
                    .iter()
                    .filter(|(position, _)| *position >= self.next_position)
                    .take(self.stream.inner.batch_limit)
                    .cloned()
                    .collect::<Vec<_>>();
                let final_position = entries.last().map(|(position, _)| *position);
                final_position.map(|position| ActivityBatch {
                    position,
                    entries: entries.into_iter().map(|(_, entry)| entry).collect(),
                })
            };
            if let Some(batch) = batch {
                self.next_position = batch.position + 1;
                return Some(batch);
            }
            if self.changed.changed().await.is_err() {
                return None;
            }
        }
    }

    pub(crate) fn host_instance_id(&self) -> &HostInstanceId {
        self.stream.host_instance_id()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies eviction rebases a lagging subscriber and exposes a position
    /// gap.
    #[tokio::test]
    async fn lagging_subscription_rebases_to_oldest_retained_entry() {
        let host = HostInstanceId::generate();
        let stream = ActivityStream::new(
            host.clone(),
            NonZeroUsize::new(2).unwrap(),
            NonZeroUsize::new(8).unwrap(),
        );
        stream.append("one");
        let mut subscription = stream.subscribe(Some((&host, 1)));
        stream.append("two");
        stream.append("three");
        stream.append("four");
        let batch = subscription.recv().await.unwrap();
        assert_eq!(batch.position, 4);
        assert_eq!(batch.entries, ["three", "four"]);
    }

    /// Verifies a cursor from another host instance starts at retained history.
    #[tokio::test]
    async fn previous_host_cursor_rebases_to_retained_history() {
        let stream = ActivityStream::new(
            HostInstanceId::generate(),
            NonZeroUsize::new(4).unwrap(),
            NonZeroUsize::new(4).unwrap(),
        );
        stream.append(7);
        let previous = HostInstanceId::generate();
        let mut subscription = stream.subscribe(Some((&previous, 99)));
        let batch = subscription.recv().await.unwrap();
        assert_eq!(batch.position, 1);
        assert_eq!(batch.entries, [7]);
    }
}
