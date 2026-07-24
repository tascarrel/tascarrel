//! Sanitized, line-addressed process log buffering.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use tascarrel_api::types::processes::ProcessLogEvent;
use tascarrel_api::types::processes::ProcessLogLine;
use tascarrel_api::types::processes::ProcessLogSource;
use tokio::sync::watch;

/// Bounded process log shared by producers and resumable subscribers.
#[derive(Clone)]
pub(crate) struct LogBuffer {
    core: Arc<LogCore>,
}

impl LogBuffer {
    /// Creates an empty log with bounded retention and event batches.
    pub(crate) fn new(capacity: NonZeroUsize, batch_capacity: NonZeroUsize) -> Self {
        let (changed, _) = watch::channel(0);
        Self {
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
        }
    }

    /// Appends one parsed line unless the log has closed.
    pub(crate) fn append(&self, source: ProcessLogSource, content: String, truncated: bool) {
        let mut state = lock(&self.core.state);
        if state.closed {
            return;
        }
        let line = state
            .last_line
            .checked_add(1)
            .expect("an open process log has line number capacity");
        state.lines.push_back(ProcessLogLine {
            line,
            source,
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
    pub(crate) fn close(&self) {
        let mut state = lock(&self.core.state);
        if state.closed {
            return;
        }
        state.closed = true;
        self.core.changed.send_replace(state.last_line);
    }

    /// Subscribes after the last line already observed by a consumer.
    pub(crate) fn subscribe(&self, last_line: Option<u64>) -> LogSubscription {
        LogSubscription {
            core: Arc::clone(&self.core),
            changed: self.core.changed.subscribe(),
            cursor: last_line.unwrap_or(0),
        }
    }
}

/// Resumable stream over one process log ring buffer.
pub(crate) struct LogSubscription {
    core: Arc<LogCore>,
    changed: watch::Receiver<u64>,
    cursor: u64,
}

impl LogSubscription {
    /// Receives the next non-empty batch of retained or live lines.
    pub(crate) async fn recv(&mut self) -> Option<ProcessLogEvent> {
        loop {
            {
                let state = lock(&self.core.state);
                if let Some(lines) = state.lines_after(self.cursor, self.core.batch_capacity) {
                    self.cursor = lines
                        .last()
                        .expect("a process log event contains at least one line")
                        .line;
                    return Some(ProcessLogEvent {
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
                .expect("a process log subscription retains the watch sender");
        }
    }
}

/// Stateful terminal-sequence parser that produces sanitized lines.
pub(crate) struct LineDecoder {
    parser: vte::Parser,
    collector: LineCollector,
}

impl LineDecoder {
    /// Creates a decoder with a maximum UTF-8 byte length for each line.
    pub(crate) fn new(max_line_bytes: usize) -> Self {
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

    /// Parses one output chunk and returns every complete line it produced.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<DecodedLine> {
        self.parser.advance(&mut self.collector, bytes);
        std::mem::take(&mut self.collector.completed)
    }

    /// Finishes a trailing line without a line terminator.
    pub(crate) fn finish(&mut self) -> Option<DecodedLine> {
        if self.collector.content.is_empty() && !self.collector.truncated {
            None
        } else {
            Some(self.collector.take_line())
        }
    }
}

/// Sanitized content and its truncation marker.
pub(crate) struct DecodedLine {
    pub(crate) content: String,
    pub(crate) truncated: bool,
}

struct LogCore {
    capacity: usize,
    batch_capacity: usize,
    state: Mutex<LogState>,
    changed: watch::Sender<u64>,
}

struct LogState {
    last_line: u64,
    lines: VecDeque<ProcessLogLine>,
    closed: bool,
}

impl LogState {
    /// Returns the next retained lines after `cursor` as one non-empty batch.
    fn lines_after(&self, cursor: u64, batch_capacity: usize) -> Option<Vec<ProcessLogLine>> {
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
    use tascarrel_api::types::processes::ProcessLogSource;

    use super::*;

    /// Verifies fragmented terminal sequences are removed before lines are
    /// published.
    #[test]
    fn decoder_sanitizes_fragmented_terminal_sequences() {
        let mut decoder = LineDecoder::new(64);
        assert!(decoder.push(b"hello \x1b[3").is_empty());
        let lines = decoder.push(b"1mred\x1b[0m\n");

        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].content, "hello red");
        assert!(!lines[0].truncated);
    }

    /// Verifies line truncation preserves later line boundaries.
    #[test]
    fn decoder_truncates_individual_lines() {
        let mut decoder = LineDecoder::new(4);
        let lines = decoder.push(b"abcdef\nok\n");

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].content, "abcd");
        assert!(lines[0].truncated);
        assert_eq!(lines[1].content, "ok");
        assert!(!lines[1].truncated);
    }

    /// Verifies a stale cursor resumes at the oldest line still retained.
    #[tokio::test]
    async fn ring_buffer_exposes_eviction_as_a_line_number_gap() {
        let log = LogBuffer::new(nonzero(2), nonzero(2));
        for content in ["one", "two", "three"] {
            log.append(ProcessLogSource::Stdout, content.to_owned(), false);
        }
        log.close();
        let mut subscription = log.subscribe(Some(0));

        let event = subscription.recv().await.unwrap();
        assert_eq!(
            event.lines.iter().map(|line| line.line).collect::<Vec<_>>(),
            [2, 3]
        );
        assert!(subscription.recv().await.is_none());
    }

    /// Verifies a retained cursor resumes at the immediately following line.
    #[tokio::test]
    async fn ring_buffer_resumes_after_a_retained_line() {
        let log = LogBuffer::new(nonzero(3), nonzero(2));
        for content in ["one", "two", "three"] {
            log.append(ProcessLogSource::Stdout, content.to_owned(), false);
        }
        log.close();
        let mut subscription = log.subscribe(Some(1));

        let event = subscription.recv().await.unwrap();
        assert_eq!(
            event.lines.iter().map(|line| line.line).collect::<Vec<_>>(),
            [2, 3]
        );
        assert!(subscription.recv().await.is_none());
    }

    /// Verifies a retained backlog is split at the configured event capacity.
    #[tokio::test]
    async fn ring_buffer_limits_each_line_batch() {
        let log = LogBuffer::new(nonzero(3), nonzero(2));
        for content in ["one", "two", "three"] {
            log.append(ProcessLogSource::Stdout, content.to_owned(), false);
        }
        log.close();
        let mut subscription = log.subscribe(None);

        let first = subscription.recv().await.unwrap();
        assert_eq!(
            first.lines.iter().map(|line| line.line).collect::<Vec<_>>(),
            [1, 2]
        );
        let second = subscription.recv().await.unwrap();
        assert_eq!(
            second
                .lines
                .iter()
                .map(|line| line.line)
                .collect::<Vec<_>>(),
            [3]
        );
        assert!(subscription.recv().await.is_none());
    }

    fn nonzero(value: usize) -> NonZeroUsize {
        NonZeroUsize::new(value).expect("test value is non-zero")
    }
}
