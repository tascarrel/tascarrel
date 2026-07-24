//! Byte-addressed process terminal output buffering.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use tascarrel_api::ProcessTerminalData;
use tascarrel_api::types::processes as api;
use tokio::sync::watch;

/// Bounded terminal byte stream shared by one producer and resumable
/// subscribers.
#[derive(Clone)]
pub(crate) struct TerminalBuffer {
    core: Arc<TerminalCore>,
}

impl TerminalBuffer {
    /// Creates an empty byte stream with bounded retention and event sizes.
    pub(crate) fn new(capacity: NonZeroUsize, event_capacity: NonZeroUsize) -> Self {
        let (changed, _) = watch::channel(0);
        Self {
            core: Arc::new(TerminalCore {
                capacity: capacity.get(),
                event_capacity: event_capacity.get(),
                state: Mutex::new(TerminalState {
                    chunks: VecDeque::new(),
                    retained_bytes: 0,
                    end_offset: 0,
                    closed: false,
                }),
                changed,
            }),
        }
    }

    /// Appends unmodified terminal bytes unless the stream has closed.
    pub(crate) fn append(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let mut state = lock(&self.core.state);
        if state.closed {
            return;
        }
        let length = u64::try_from(data.len()).expect("terminal output chunk length fits in u64");
        let end_offset = state
            .end_offset
            .checked_add(length)
            .expect("terminal output offset fits in u64 for the process lifetime");
        let start_offset = state.end_offset;
        state.chunks.push_back(TerminalChunk {
            start_offset,
            data: Arc::from(data),
        });
        state.retained_bytes += data.len();
        state.end_offset = end_offset;
        state.evict_to(self.core.capacity);
        self.core.changed.send_replace(end_offset);
    }

    /// Marks the byte stream complete and wakes every subscriber.
    pub(crate) fn close(&self) {
        let mut state = lock(&self.core.state);
        if state.closed {
            return;
        }
        state.closed = true;
        self.core.changed.send_replace(state.end_offset);
    }

    /// Subscribes from the next byte offset expected by a consumer.
    pub(crate) fn subscribe(
        &self,
        offset: Option<u64>,
    ) -> Result<TerminalSubscription, TerminalCursorError> {
        let offset = offset.unwrap_or(0);
        let catch_up_end = lock(&self.core.state).end_offset;
        if offset > catch_up_end {
            return Err(TerminalCursorError {
                requested: offset,
                end_offset: catch_up_end,
            });
        }
        Ok(TerminalSubscription {
            core: Arc::clone(&self.core),
            changed: self.core.changed.subscribe(),
            cursor: offset,
            catch_up_end,
            caught_up: false,
        })
    }
}

/// A terminal subscription cursor referred to output that does not exist yet.
#[derive(Debug)]
pub(crate) struct TerminalCursorError {
    pub(crate) requested: u64,
    pub(crate) end_offset: u64,
}

/// Resumable stream over one process terminal byte ring.
pub(crate) struct TerminalSubscription {
    core: Arc<TerminalCore>,
    changed: watch::Receiver<u64>,
    cursor: u64,
    catch_up_end: u64,
    caught_up: bool,
}

impl TerminalSubscription {
    /// Receives the next output range or replay-boundary marker.
    pub(crate) async fn recv(&mut self) -> Option<api::ProcessTerminalEvent> {
        loop {
            {
                let state = lock(&self.core.state);
                let ceiling = if self.caught_up {
                    state.end_offset
                } else {
                    self.catch_up_end
                };
                if let Some(output) =
                    state.output_after(self.cursor, ceiling, self.core.event_capacity)
                {
                    self.cursor = output.end_offset;
                    return Some(api::ProcessTerminalEvent {
                        update: api::ProcessTerminalUpdate::Output(output),
                    });
                }
                if !self.caught_up {
                    self.cursor = self.catch_up_end;
                    self.caught_up = true;
                    return Some(api::ProcessTerminalEvent {
                        update: api::ProcessTerminalUpdate::CaughtUp(
                            api::ProcessTerminalCaughtUp {
                                offset: self.catch_up_end,
                            },
                        ),
                    });
                }
                if state.closed {
                    return None;
                }
            }
            self.changed
                .changed()
                .await
                .expect("a terminal subscription retains the watch sender");
        }
    }
}

struct TerminalCore {
    capacity: usize,
    event_capacity: usize,
    state: Mutex<TerminalState>,
    changed: watch::Sender<u64>,
}

struct TerminalState {
    chunks: VecDeque<TerminalChunk>,
    retained_bytes: usize,
    end_offset: u64,
    closed: bool,
}

impl TerminalState {
    fn evict_to(&mut self, capacity: usize) {
        while self.retained_bytes > capacity {
            let excess = self.retained_bytes - capacity;
            let front = self
                .chunks
                .front_mut()
                .expect("retained terminal bytes have a front chunk");
            if front.data.len() <= excess {
                self.retained_bytes -= front.data.len();
                self.chunks.pop_front();
            } else {
                let skipped = u64::try_from(excess).expect("retention excess fits in u64");
                front.start_offset += skipped;
                front.data = Arc::from(&front.data[excess..]);
                self.retained_bytes -= excess;
            }
        }
    }

    fn output_after(
        &self,
        cursor: u64,
        ceiling: u64,
        event_capacity: usize,
    ) -> Option<api::ProcessTerminalOutput> {
        let retained_start = self
            .chunks
            .front()
            .map_or(self.end_offset, |chunk| chunk.start_offset);
        let requested = cursor.max(retained_start);
        if requested >= ceiling {
            return None;
        }
        let chunk = self.chunks.iter().find(|chunk| {
            let length =
                u64::try_from(chunk.data.len()).expect("terminal chunk length fits in u64");
            chunk.start_offset + length > requested
        })?;
        let chunk_index = usize::try_from(requested - chunk.start_offset)
            .expect("terminal chunk index fits in usize");
        let available_to_ceiling = match usize::try_from(ceiling - requested) {
            Ok(available) => available,
            Err(_) => usize::MAX,
        };
        let length = (chunk.data.len() - chunk_index)
            .min(event_capacity)
            .min(available_to_ceiling);
        let end_offset =
            requested + u64::try_from(length).expect("terminal output event length fits in u64");
        Some(api::ProcessTerminalOutput {
            start_offset: requested,
            end_offset,
            data: ProcessTerminalData::from(&chunk.data[chunk_index..chunk_index + length]),
        })
    }
}

struct TerminalChunk {
    start_offset: u64,
    data: Arc<[u8]>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies subscribers receive byte-exact output even when events split a
    /// UTF-8 sequence and include bytes that are not valid UTF-8.
    #[tokio::test]
    async fn preserves_binary_data_across_arbitrary_event_boundaries() {
        let buffer = TerminalBuffer::new(nonzero(64), nonzero(3));
        buffer.append(&[0xf0, 0x9f]);
        buffer.append(&[0x98, 0x80, 0xff]);
        let mut subscription = buffer.subscribe(None).unwrap();
        let mut bytes = Vec::new();

        loop {
            let event = subscription.recv().await.unwrap();
            match event.update {
                api::ProcessTerminalUpdate::Output(output) => {
                    bytes.extend_from_slice(output.data.as_bytes());
                }
                api::ProcessTerminalUpdate::CaughtUp(marker) => {
                    assert_eq!(marker.offset, 5);
                    break;
                }
            }
        }

        assert_eq!(bytes, [0xf0, 0x9f, 0x98, 0x80, 0xff]);
    }

    /// Verifies retention eviction is represented by the first delivered byte
    /// offset rather than by altering the retained stream.
    #[tokio::test]
    async fn exposes_an_offset_gap_after_retention_eviction() {
        let buffer = TerminalBuffer::new(nonzero(4), nonzero(16));
        buffer.append(b"abc");
        buffer.append(b"def");
        let mut subscription = buffer.subscribe(None).unwrap();
        let mut start = None;
        let mut bytes = Vec::new();

        loop {
            let event = subscription.recv().await.unwrap();
            match event.update {
                api::ProcessTerminalUpdate::Output(output) => {
                    start.get_or_insert(output.start_offset);
                    bytes.extend_from_slice(output.data.as_bytes());
                }
                api::ProcessTerminalUpdate::CaughtUp(marker) => {
                    assert_eq!(marker.offset, 6);
                    break;
                }
            }
        }

        assert_eq!(start, Some(2));
        assert_eq!(bytes, b"cdef");
    }

    /// Verifies every subscription emits its fixed replay boundary before
    /// delivering output appended afterward.
    #[tokio::test]
    async fn separates_replay_from_subsequent_live_output() {
        let buffer = TerminalBuffer::new(nonzero(64), nonzero(16));
        buffer.append(b"old");
        let mut subscription = buffer.subscribe(None).unwrap();

        assert!(matches!(
            subscription.recv().await.unwrap().update,
            api::ProcessTerminalUpdate::Output(_)
        ));
        assert!(matches!(
            subscription.recv().await.unwrap().update,
            api::ProcessTerminalUpdate::CaughtUp(_)
        ));
        buffer.append(b"new");
        let api::ProcessTerminalUpdate::Output(output) = subscription.recv().await.unwrap().update
        else {
            panic!("live terminal output event expected");
        };
        assert_eq!(output.start_offset, 3);
        assert_eq!(output.data.as_bytes(), b"new");
    }

    /// Verifies closing a terminal stream releases a subscriber waiting for
    /// live output.
    #[tokio::test]
    async fn closing_the_buffer_wakes_a_live_subscriber() {
        let buffer = TerminalBuffer::new(nonzero(64), nonzero(16));
        let mut subscription = buffer.subscribe(None).unwrap();
        assert!(matches!(
            subscription.recv().await.unwrap().update,
            api::ProcessTerminalUpdate::CaughtUp(_)
        ));
        let waiting = tokio::spawn(async move { subscription.recv().await });
        tokio::task::yield_now().await;

        buffer.close();

        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
    }

    fn nonzero(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).unwrap()
    }
}
