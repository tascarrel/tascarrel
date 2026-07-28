//! Sanitized, line-addressed workspace VM logs.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use tascarrel_api::types::workspaces as api;
use tokio::sync::watch;

/// Read side of one bounded workspace VM log.
#[derive(Clone)]
pub(crate) struct WorkspaceVmLog {
    core: Arc<LogCore>,
}

impl WorkspaceVmLog {
    /// Creates an empty log and its single logical writer.
    pub(crate) fn new(
        capacity: NonZeroUsize,
        batch_capacity: NonZeroUsize,
        max_line_bytes: NonZeroUsize,
    ) -> (Self, WorkspaceVmLogWriter) {
        let (changed, _) = watch::channel(0);
        let log = Self {
            core: Arc::new(LogCore {
                capacity: capacity.get(),
                batch_capacity: batch_capacity.get(),
                state: Mutex::new(LogState {
                    last_line: 0,
                    lines: VecDeque::with_capacity(capacity.get()),
                    closed: false,
                }),
                changed,
            }),
        };
        let writer = WorkspaceVmLogWriter {
            state: Arc::new(Mutex::new(WriterState {
                log: log.clone(),
                decoder: LineDecoder::new(max_line_bytes.get()),
                closed: false,
            })),
        };
        (log, writer)
    }

    /// Subscribes after the last line already observed by a consumer.
    pub(crate) fn subscribe(&self, last_line: Option<u64>) -> WorkspaceVmLogSubscription {
        WorkspaceVmLogSubscription {
            core: Arc::clone(&self.core),
            changed: self.core.changed.subscribe(),
            cursor: last_line.unwrap_or_default(),
        }
    }

    /// Appends one decoded line unless the log has closed.
    fn append(&self, content: String, truncated: bool) {
        let mut state = lock(&self.core.state);
        if state.closed {
            return;
        }
        let line = state
            .last_line
            .checked_add(1)
            .expect("an open workspace VM log has line number capacity");
        state.lines.push_back(api::WorkspaceVmLogLine {
            line,
            content: content.into(),
            truncated,
        });
        if state.lines.len() > self.core.capacity {
            state.lines.pop_front();
        }
        state.last_line = line;
        if line == u64::MAX {
            state.closed = true;
        }
        self.core.changed.send_replace(line);
    }

    /// Marks the log complete and wakes every subscriber.
    fn close(&self) {
        let mut state = lock(&self.core.state);
        if state.closed {
            return;
        }
        state.closed = true;
        self.core.changed.send_replace(state.last_line);
    }
}

/// Parses byte chunks and writes complete lines to one workspace VM log.
#[derive(Clone)]
pub(crate) struct WorkspaceVmLogWriter {
    state: Arc<Mutex<WriterState>>,
}

impl WorkspaceVmLogWriter {
    /// Parses one serial-console byte chunk.
    pub(crate) fn write(&self, bytes: &[u8]) {
        let mut state = lock(&self.state);
        if state.closed {
            return;
        }
        for line in state.decoder.push(bytes) {
            state.log.append(line.content, line.truncated);
        }
    }

    /// Finishes a trailing line and closes the log.
    pub(crate) fn close(&self) {
        let mut state = lock(&self.state);
        if state.closed {
            return;
        }
        if let Some(line) = state.decoder.finish() {
            state.log.append(line.content, line.truncated);
        }
        state.closed = true;
        state.log.close();
    }
}

/// Resumable stream over one workspace VM log ring buffer.
pub struct WorkspaceVmLogSubscription {
    core: Arc<LogCore>,
    changed: watch::Receiver<u64>,
    cursor: u64,
}

impl WorkspaceVmLogSubscription {
    /// Receives the next non-empty batch of retained or live lines.
    ///
    /// # Panics
    ///
    /// Panics if the log returns an empty batch or drops its watch sender while
    /// the subscription is still active.
    pub async fn recv(&mut self) -> Option<api::WorkspaceVmLogEvent> {
        loop {
            {
                let state = lock(&self.core.state);
                if let Some(lines) = state.lines_after(self.cursor, self.core.batch_capacity) {
                    self.cursor = lines
                        .last()
                        .expect("a workspace VM log event contains at least one line")
                        .line;
                    return Some(api::WorkspaceVmLogEvent {
                        lines: lines.into(),
                    });
                }
                if state.closed {
                    return None;
                }
            }
            self.changed
                .changed()
                .await
                .expect("a workspace VM log subscription retains the watch sender");
        }
    }
}

struct WriterState {
    log: WorkspaceVmLog,
    decoder: LineDecoder,
    closed: bool,
}

struct LogCore {
    capacity: usize,
    batch_capacity: usize,
    state: Mutex<LogState>,
    changed: watch::Sender<u64>,
}

struct LogState {
    last_line: u64,
    lines: VecDeque<api::WorkspaceVmLogLine>,
    closed: bool,
}

impl LogState {
    /// Returns the next retained lines after `cursor` as one non-empty batch.
    fn lines_after(
        &self,
        cursor: u64,
        batch_capacity: usize,
    ) -> Option<Vec<api::WorkspaceVmLogLine>> {
        if self.lines.is_empty() || cursor >= self.last_line {
            return None;
        }
        let retained = u64::try_from(self.lines.len()).expect("retained line count fits in u64");
        let first_line = self.last_line - retained + 1;
        let requested = cursor.saturating_add(1).max(first_line);
        let index = usize::try_from(requested - first_line).expect("line index fits in usize");
        Some(
            self.lines
                .iter()
                .skip(index)
                .take(batch_capacity)
                .cloned()
                .collect(),
        )
    }
}

/// Stateful terminal-sequence parser that produces sanitized lines.
struct LineDecoder {
    parser: vte::Parser,
    collector: LineCollector,
}

impl LineDecoder {
    fn new(max_line_bytes: usize) -> Self {
        Self {
            parser: vte::Parser::new(),
            collector: LineCollector {
                max_line_bytes,
                content: String::new(),
                truncated: false,
                completed: Vec::new(),
            },
        }
    }

    fn push(&mut self, bytes: &[u8]) -> Vec<DecodedLine> {
        self.parser.advance(&mut self.collector, bytes);
        std::mem::take(&mut self.collector.completed)
    }

    fn finish(&mut self) -> Option<DecodedLine> {
        if self.collector.content.is_empty() && !self.collector.truncated {
            None
        } else {
            Some(self.collector.take_line())
        }
    }
}

struct DecodedLine {
    content: String,
    truncated: bool,
}

struct LineCollector {
    max_line_bytes: usize,
    content: String,
    truncated: bool,
    completed: Vec<DecodedLine>,
}

impl LineCollector {
    fn take_line(&mut self) -> DecodedLine {
        DecodedLine {
            content: std::mem::take(&mut self.content),
            truncated: std::mem::take(&mut self.truncated),
        }
    }
}

impl vte::Perform for LineCollector {
    fn print(&mut self, character: char) {
        if self.content.len() + character.len_utf8() <= self.max_line_bytes {
            self.content.push(character);
        } else {
            self.truncated = true;
        }
    }

    fn execute(&mut self, byte: u8) {
        if byte == b'\n' {
            let line = self.take_line();
            self.completed.push(line);
        }
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

    /// Verifies stale cursors expose eviction through a line-number gap.
    #[tokio::test]
    async fn log_resumes_at_the_oldest_retained_line() {
        let (log, writer) = WorkspaceVmLog::new(nonzero(2), nonzero(2), nonzero(64));
        writer.write(b"one\ntwo\nthree\n");
        writer.close();
        let mut subscription = log.subscribe(Some(0));

        let event = subscription.recv().await.expect("retained log event");
        assert_eq!(
            event.lines.iter().map(|line| line.line).collect::<Vec<_>>(),
            [2, 3]
        );
        assert!(subscription.recv().await.is_none());
    }

    /// Verifies terminal controls are removed and oversized lines are marked.
    #[tokio::test]
    async fn log_sanitizes_and_truncates_serial_output() {
        let (log, writer) = WorkspaceVmLog::new(nonzero(2), nonzero(2), nonzero(4));
        writer.write(b"\x1b[31mabcdef\x1b[0m\n");
        writer.close();
        let mut subscription = log.subscribe(None);

        let event = subscription.recv().await.expect("retained log event");
        let [line] = event.lines.as_ref() else {
            panic!("one serial-console line is retained");
        };
        assert_eq!(line.content, "abcd");
        assert!(line.truncated);
    }

    fn nonzero(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).expect("test value is non-zero")
    }
}
