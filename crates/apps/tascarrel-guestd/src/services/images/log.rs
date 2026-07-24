//! Sanitized, line-addressed image generation logs.

use std::collections::VecDeque;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;

use tascarrel_api::types::images as api;
use tokio::sync::watch;

/// Writes `BuildKit` and setup output into one ordered image log.
///
/// Clones share decoders and preserve the order in which complete lines are
/// appended. Output after the generation closes its log is ignored.
#[derive(Clone)]
pub(crate) struct ImageLogWriter {
    state: Arc<Mutex<WriterState>>,
}

impl ImageLogWriter {
    /// Creates a writer with independent decoders for both generation stages.
    pub(crate) fn new(log: LogBuffer, max_line_bytes: NonZeroUsize) -> Self {
        Self {
            state: Arc::new(Mutex::new(WriterState {
                log,
                buildkit: LineDecoder::new(max_line_bytes.get()),
                setup: LineDecoder::new(max_line_bytes.get()),
                closed: false,
            })),
        }
    }

    /// Parses one output chunk for a generation stage.
    pub(crate) fn write(&self, source: &api::ImageLogSource, bytes: &[u8]) {
        let mut state = lock(&self.state);
        if state.closed {
            return;
        }
        let lines = match source {
            api::ImageLogSource::BuildKit => state.buildkit.push(bytes),
            api::ImageLogSource::Setup => state.setup.push(bytes),
        };
        for line in lines {
            state
                .log
                .append(source.clone(), line.content, line.truncated);
        }
    }

    /// Finishes both stages and closes the shared log.
    pub(crate) fn close(&self) {
        let mut state = lock(&self.state);
        if state.closed {
            return;
        }
        if let Some(line) = state.buildkit.finish() {
            state
                .log
                .append(api::ImageLogSource::BuildKit, line.content, line.truncated);
        }
        if let Some(line) = state.setup.finish() {
            state
                .log
                .append(api::ImageLogSource::Setup, line.content, line.truncated);
        }
        state.closed = true;
        state.log.close();
    }
}

/// Bounded image log shared by its writer and resumable subscribers.
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
    fn append(&self, source: api::ImageLogSource, content: String, truncated: bool) {
        let mut state = lock(&self.core.state);
        if state.closed {
            return;
        }
        let line = state
            .last_line
            .checked_add(1)
            .expect("an open image log has line number capacity");
        state.lines.push_back(api::ImageLogLine {
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
            cursor: last_line.unwrap_or_default(),
        }
    }
}

/// Resumable stream over one image log ring buffer.
pub(crate) struct LogSubscription {
    core: Arc<LogCore>,
    changed: watch::Receiver<u64>,
    cursor: u64,
}

impl LogSubscription {
    /// Receives the next non-empty batch of retained or live lines.
    pub(crate) async fn recv(&mut self) -> Option<api::ImageLogEvent> {
        loop {
            {
                let state = lock(&self.core.state);
                if let Some(lines) = state.lines_after(self.cursor, self.core.batch_capacity) {
                    self.cursor = lines
                        .last()
                        .expect("an image log event contains at least one line")
                        .line;
                    return Some(api::ImageLogEvent {
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
                .expect("an image log subscription retains the watch sender");
        }
    }
}

struct WriterState {
    log: LogBuffer,
    buildkit: LineDecoder,
    setup: LineDecoder,
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
    lines: VecDeque<api::ImageLogLine>,
    closed: bool,
}

impl LogState {
    /// Returns the next retained lines after `cursor` as one non-empty batch.
    fn lines_after(&self, cursor: u64, batch_capacity: usize) -> Option<Vec<api::ImageLogLine>> {
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
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies both generation stages share one ordered line sequence.
    #[tokio::test]
    async fn writer_combines_buildkit_and_setup_lines() {
        let log = LogBuffer::new(
            NonZeroUsize::new(8).expect("test capacity is non-zero"),
            NonZeroUsize::new(8).expect("test batch capacity is non-zero"),
        );
        let writer = ImageLogWriter::new(
            log.clone(),
            NonZeroUsize::new(64).expect("test line limit is non-zero"),
        );
        writer.write(&api::ImageLogSource::BuildKit, b"build\n");
        writer.write(&api::ImageLogSource::Setup, b"setup\n");
        writer.close();
        let mut subscription = log.subscribe(None);

        let event = subscription.recv().await.expect("log lines are retained");
        let [build, setup] = event.lines.as_ref() else {
            panic!("both retained lines are returned in one batch");
        };
        assert_eq!(build.line, 1);
        assert_eq!(build.source, api::ImageLogSource::BuildKit);
        assert_eq!(build.content, "build");
        assert_eq!(setup.line, 2);
        assert_eq!(setup.source, api::ImageLogSource::Setup);
        assert_eq!(setup.content, "setup");
        assert!(subscription.recv().await.is_none());
    }

    /// Verifies stale cursors resume at the oldest retained image line.
    #[tokio::test]
    async fn ring_buffer_exposes_eviction_as_a_line_gap() {
        let log = LogBuffer::new(
            NonZeroUsize::new(2).expect("test capacity is non-zero"),
            NonZeroUsize::new(2).expect("test batch capacity is non-zero"),
        );
        let writer = ImageLogWriter::new(
            log.clone(),
            NonZeroUsize::new(64).expect("test line limit is non-zero"),
        );
        writer.write(&api::ImageLogSource::BuildKit, b"one\ntwo\nthree\n");
        writer.close();
        let mut subscription = log.subscribe(Some(0));

        let event = subscription.recv().await.expect("retained lines");
        assert_eq!(
            event.lines.iter().map(|line| line.line).collect::<Vec<_>>(),
            [2, 3]
        );
        assert!(subscription.recv().await.is_none());
    }

    /// Verifies a retained backlog is split at the configured event capacity.
    #[tokio::test]
    async fn ring_buffer_limits_each_line_batch() {
        let log = LogBuffer::new(
            NonZeroUsize::new(3).expect("test capacity is non-zero"),
            NonZeroUsize::new(2).expect("test batch capacity is non-zero"),
        );
        let writer = ImageLogWriter::new(
            log.clone(),
            NonZeroUsize::new(64).expect("test line limit is non-zero"),
        );
        writer.write(&api::ImageLogSource::BuildKit, b"one\ntwo\nthree\n");
        writer.close();
        let mut subscription = log.subscribe(None);

        let first = subscription.recv().await.expect("first retained batch");
        assert_eq!(
            first.lines.iter().map(|line| line.line).collect::<Vec<_>>(),
            [1, 2]
        );
        let second = subscription.recv().await.expect("second retained batch");
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

    /// Verifies terminal controls are removed and oversized lines are marked.
    #[tokio::test]
    async fn writer_sanitizes_and_truncates_lines() {
        let log = LogBuffer::new(
            NonZeroUsize::new(2).expect("test capacity is non-zero"),
            NonZeroUsize::new(2).expect("test batch capacity is non-zero"),
        );
        let writer = ImageLogWriter::new(
            log.clone(),
            NonZeroUsize::new(4).expect("test line limit is non-zero"),
        );
        writer.write(&api::ImageLogSource::Setup, b"\x1b[31mabcdef\x1b[0m\n");
        writer.close();
        let mut subscription = log.subscribe(None);

        let event = subscription.recv().await.expect("sanitized line batch");
        let [line] = event.lines.as_ref() else {
            panic!("one sanitized line is returned");
        };
        assert_eq!(line.content, "abcd");
        assert!(line.truncated);
    }
}
