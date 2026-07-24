//! Native filesystem event delivery for workspace inventory changes.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use notify::Event;
use notify::RecommendedWatcher;
use notify::RecursiveMode;
use notify::Watcher as _;
use reportify::ErrorExt as _;
use reportify::Report;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tracing::debug;

/// Keeps the platform watcher alive and exposes its bounded event stream.
pub(crate) struct WatchEvents {
    receiver: mpsc::Receiver<WatchMessage>,
    overflowed: Arc<AtomicBool>,
    watcher: Option<RecommendedWatcher>,
}

impl WatchEvents {
    /// Opens the platform-recommended watcher for workspace-root entries.
    pub(crate) fn open(root: &Path, capacity: usize) -> Result<Self, Report<WatcherError>> {
        let (sender, receiver) = mpsc::channel(capacity);
        let mut events = Self::from_receiver(receiver);
        let callback_overflowed = Arc::clone(&events.overflowed);
        let mut watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
            let message = match result {
                Ok(event) => WatchMessage::Event(event),
                Err(error) => WatchMessage::Error(error),
            };
            match sender.try_send(message) {
                Ok(()) => {}
                Err(TrySendError::Closed(_)) => {
                    debug!("workspace inventory watcher event channel is closed");
                }
                Err(TrySendError::Full(_)) => {
                    callback_overflowed.store(true, Ordering::Release);
                }
            }
        })
        .map_err(|error| WatcherError::Initialize.report().message(error.to_string()))?;
        watcher
            .watch(root, RecursiveMode::NonRecursive)
            .map_err(|error| WatcherError::Watch.report().message(error.to_string()))?;
        events.watcher = Some(watcher);
        Ok(events)
    }

    /// Receives the next native watcher message.
    pub(crate) async fn recv(&mut self) -> Option<WatchMessage> {
        self.receiver.recv().await
    }

    /// Returns whether native events overflowed since the preceding call.
    pub(crate) fn take_overflow(&self) -> bool {
        self.overflowed.swap(false, Ordering::AcqRel)
    }

    /// Constructs an event stream from its bounded receiver.
    pub(crate) fn from_receiver(receiver: mpsc::Receiver<WatchMessage>) -> Self {
        Self {
            receiver,
            overflowed: Arc::new(AtomicBool::new(false)),
            watcher: None,
        }
    }
}

impl Drop for WatchEvents {
    fn drop(&mut self) {
        drop(self.watcher.take());
    }
}

/// Caller-relevant native watcher initialization failures.
#[derive(Clone, Copy, Debug, Error)]
pub(crate) enum WatcherError {
    /// The platform-recommended watcher could not be initialized.
    #[error("failed to initialize workspace inventory watcher")]
    Initialize,
    /// The workspace configuration root could not be watched.
    #[error("failed to watch workspace configuration directory")]
    Watch,
}

/// Native event or watcher failure delivered to the asynchronous service.
pub(crate) enum WatchMessage {
    /// A native filesystem change notification.
    Event(Event),
    /// A failure reported asynchronously by the native watcher.
    Error(notify::Error),
}
