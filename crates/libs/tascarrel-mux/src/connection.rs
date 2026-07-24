//! Logical connection handles, channels, and their shared bounded state.
//!
//! [`MuxHandle`] opens outgoing channels, [`Incoming`] yields peer requests,
//! and [`Channel`] exposes accepted streams through Tokio's I/O traits.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt;
use std::future::poll_fn;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::Weak;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

use reportify::ErrorExt as _;
use reportify::ResultExt as _;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::ReadBuf;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tracing::debug;
use tracing::warn;

use crate::Config;
use crate::Error;
use crate::Result;
use crate::Role;
use crate::driver::Driver;
use crate::protocol::Frame;
use crate::terminal_report;

/// A clonable handle used to open logical channels.
#[derive(Clone, Debug)]
pub struct MuxHandle {
    pub(crate) core: Arc<Core>,
}

/// The receiving half for peer channel requests.
#[derive(Debug)]
pub struct Incoming {
    receiver: mpsc::Receiver<IncomingRequest>,
}

/// A pending peer channel request.
#[derive(Debug)]
pub struct IncomingRequest {
    pub(crate) id: u32,
    pub(crate) endpoint: String,
    pub(crate) core: Weak<Core>,
    pub(crate) resolved: bool,
}

/// One bidirectional logical byte stream.
///
/// Dropping a channel preserves queued output and initiates a graceful close
/// in the connection driver. Unread peer data is drained without buffering so
/// the peer can finish its write half.
pub struct Channel {
    pub(crate) state: Arc<ChannelState>,
    pub(crate) core: Weak<Core>,
    pub(crate) buffer_limit: usize,
}

/// Creates a multiplex connection over an ordered, reliable byte stream.
///
/// The returned driver must run continuously while either public connection
/// handle is in use.
///
/// # Errors
///
/// Returns [`Error::InvalidConfig`] when a configured value is zero where
/// prohibited, internally inconsistent, or not representable by the wire or
/// runtime.
#[tracing::instrument(
    name = "tascarrel_mux.connect",
    level = "debug",
    skip(io, role, config),
    fields(
        ?role,
        initial_byte_window = config.initial_byte_window,
        initial_frame_window = config.initial_frame_window,
        max_frame_size = config.max_frame_size,
        max_channels = config.max_channels,
    ),
    err(level = "debug")
)]
pub fn connect<T>(io: T, role: Role, config: Config) -> Result<(Driver<T>, MuxHandle, Incoming)> {
    config.validate()?;
    let (incoming_tx, incoming_rx) = mpsc::channel(config.max_channels);
    let core = Arc::new(Core {
        inner: Mutex::new(CoreInner {
            role,
            next_id: role.first_id(),
            last_peer_id: 0,
            channels: BTreeMap::new(),
            pending_outgoing: HashMap::new(),
            pending_incoming: HashMap::new(),
            controls: VecDeque::new(),
            tombstones: BTreeSet::new(),
            tombstone_order: VecDeque::new(),
            incoming_tx: Some(incoming_tx),
            terminal: None,
            config,
        }),
        notify: Notify::new(),
    });
    Ok((
        Driver {
            io: Some(io),
            core: Arc::clone(&core),
        },
        MuxHandle {
            core: Arc::clone(&core),
        },
        Incoming {
            receiver: incoming_rx,
        },
    ))
}

impl MuxHandle {
    /// Opens a logical channel and waits for the peer to accept or reject it.
    ///
    /// # Errors
    ///
    /// Returns an error when the endpoint or connection limits are invalid,
    /// the peer rejects the request, or the underlying connection closes.
    #[tracing::instrument(
        name = "tascarrel_mux.channel.open",
        level = "debug",
        skip(self, endpoint),
        fields(
            channel_id = tracing::field::Empty,
            endpoint_length = tracing::field::Empty,
        ),
        err(level = "debug")
    )]
    pub async fn open(&self, endpoint: impl Into<String>) -> Result<Channel> {
        let endpoint = endpoint.into();
        tracing::Span::current().record("endpoint_length", endpoint.len());
        let (sender, receiver) = oneshot::channel();
        let state = {
            let mut inner = lock(&self.core.inner);
            if let Some(error) = &inner.terminal {
                return Err(terminal_report(error));
            }
            if endpoint.is_empty() || endpoint.len() > inner.config.max_endpoint_size {
                return Err(Error::InvalidInput.report());
            }
            if inner.channel_count() >= inner.config.max_channels {
                return Err(Error::ResourceExhausted.report());
            }
            let id = inner.next_id;
            tracing::Span::current().record("channel_id", id);
            inner.next_id = id
                .checked_add(2)
                .ok_or_else(|| Error::ResourceExhausted.report())?;
            let state = Arc::new(ChannelState {
                id,
                inner: Mutex::new(ChannelInner::new(&inner.config)),
            });
            let frame = Frame::Open {
                id,
                byte_window: inner.config.initial_byte_window,
                frame_window: inner.config.initial_frame_window,
                max_frame_size: inner.config.max_frame_size,
                endpoint,
            };
            inner.enqueue_control(frame)?;
            inner.channels.insert(id, Arc::clone(&state));
            inner.pending_outgoing.insert(id, sender);
            state
        };
        let mut cancellation = OpenCancellation {
            state,
            core: Arc::downgrade(&self.core),
            armed: true,
        };
        self.core.notify.notify_one();
        let result = receiver
            .await
            .unwrap_or_else(|_| Err(Error::ConnectionClosed.report()));
        cancellation.disarm();
        result
    }
}

impl Incoming {
    /// Waits for the next peer channel request.
    #[tracing::instrument(
        name = "tascarrel_mux.incoming.recv",
        level = "trace",
        skip(self),
        fields(channel_id = tracing::field::Empty)
    )]
    pub async fn recv(&mut self) -> Option<IncomingRequest> {
        while let Some(mut request) = self.receiver.recv().await {
            let pending = request
                .core
                .upgrade()
                .is_some_and(|core| lock(&core.inner).pending_incoming.contains_key(&request.id));
            if pending {
                tracing::Span::current().record("channel_id", request.id);
                return Some(request);
            }
            // A RESET or connection failure can invalidate a request already
            // in the bounded incoming queue. Do not expose stale requests or
            // auto-reject them a second time.
            request.resolved = true;
        }
        None
    }
}

impl IncomingRequest {
    /// The endpoint supplied by the opener.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Accepts the channel and advertises this side's configured windows.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection closed or its bounded control queue
    /// cannot accept the response.
    #[tracing::instrument(
        name = "tascarrel_mux.channel.accept",
        level = "debug",
        skip(self),
        fields(channel_id = self.id, endpoint_length = self.endpoint.len()),
        err(level = "debug")
    )]
    pub fn accept(mut self) -> Result<Channel> {
        let core = self
            .core
            .upgrade()
            .ok_or_else(|| Error::ConnectionClosed.report())?;
        let channel = {
            let mut inner = lock(&core.inner);
            if let Some(error) = &inner.terminal {
                return Err(terminal_report(error));
            }
            let pending = inner
                .pending_incoming
                .remove(&self.id)
                .ok_or_else(|| Error::ConnectionClosed.report())?;
            let state = Arc::new(ChannelState {
                id: self.id,
                inner: Mutex::new(ChannelInner::new(&inner.config)),
            });
            {
                let mut channel = lock(&state.inner);
                channel.send_bytes = pending.byte_window;
                channel.send_frames = pending.frame_window;
                channel.send_bytes_max = pending.byte_window;
                channel.send_frames_max = pending.frame_window;
                channel.send_frame_size = pending.max_frame_size;
            }
            let frame = Frame::Accept {
                id: self.id,
                byte_window: inner.config.initial_byte_window,
                frame_window: inner.config.initial_frame_window,
                max_frame_size: inner.config.max_frame_size,
            };
            if let Err(error) = inner.enqueue_control(frame) {
                inner.pending_incoming.insert(self.id, pending);
                return Err(error);
            }
            inner.channels.insert(self.id, Arc::clone(&state));
            Channel {
                state,
                core: Arc::downgrade(&core),
                buffer_limit: usize::try_from(inner.config.initial_byte_window)
                    .map_err(|_| Error::InvalidConfig)
                    .report()?,
            }
        };
        self.resolved = true;
        core.notify.notify_one();
        Ok(channel)
    }

    /// Rejects the channel with a short, peer-visible reason.
    ///
    /// # Errors
    ///
    /// Returns an error for an oversized reason, a closed connection, or a
    /// full bounded control queue.
    #[tracing::instrument(
        name = "tascarrel_mux.channel.reject",
        level = "debug",
        skip(self, reason),
        fields(
            channel_id = self.id,
            reason_length = tracing::field::Empty,
        ),
        err(level = "debug")
    )]
    pub fn reject(mut self, reason: impl AsRef<[u8]>) -> Result<()> {
        let reason = reason.as_ref();
        tracing::Span::current().record("reason_length", reason.len());
        let core = self
            .core
            .upgrade()
            .ok_or_else(|| Error::ConnectionClosed.report())?;
        reject_request(&core, self.id, reason)?;
        self.resolved = true;
        Ok(())
    }
}

impl Drop for IncomingRequest {
    fn drop(&mut self) {
        if self.resolved {
            return;
        }
        let Some(core) = self.core.upgrade() else {
            return;
        };
        if let Err(report) = reject_request(&core, self.id, b"request dropped") {
            if report.error() == &Error::ResourceExhausted {
                core.fail(report.error());
            } else {
                debug!(channel_id = self.id, error = %report, "could not auto-reject channel");
            }
        }
    }
}

impl fmt::Debug for Channel {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Channel")
            .field("id", &self.state.id)
            .finish_non_exhaustive()
    }
}

impl Channel {
    /// Returns the connection-local channel identifier.
    #[must_use]
    pub fn id(&self) -> u32 {
        self.state.id
    }

    /// Gracefully closes both stream directions and waits for the peer to
    /// release its channel.
    ///
    /// Unlike [`AsyncWrite::poll_shutdown`], which only closes this side's
    /// write half, this method confirms that the peer's channel handler has
    /// finished. Callers can then tear down a one-shot multiplex connection
    /// without racing peer-side cleanup.
    ///
    /// # Errors
    ///
    /// Returns a transport report when the multiplex connection or channel
    /// fails before the close handshake completes.
    pub async fn close(&mut self) -> Result<()> {
        {
            let mut inner = lock(&self.state.inner);
            if let Some(error) = &inner.terminal {
                return Err(terminal_report(error));
            }
            inner.local_fin_requested = true;
            inner.local_close_requested = true;
        }
        self.notify();
        poll_fn(|context| {
            let mut inner = lock(&self.state.inner);
            if let Some(error) = &inner.terminal {
                return Poll::Ready(Err(terminal_report(error)));
            }
            if inner.closed_sent && inner.remote_closed {
                return Poll::Ready(Ok(()));
            }
            set_waker(&mut inner.close_waker, context.waker());
            Poll::Pending
        })
        .await
    }

    /// Wakes the connection driver after channel state changes.
    fn notify(&self) {
        if let Some(core) = self.core.upgrade() {
            core.notify.notify_one();
        }
    }
}

impl AsyncRead for Channel {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut inner = lock(&self.state.inner);
        if let Some(error) = &inner.terminal {
            return Poll::Ready(Err(channel_io_error(error)));
        }
        if buffer.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        let Some(front) = inner.inbound.front_mut() else {
            if inner.remote_fin {
                return Poll::Ready(Ok(()));
            }
            set_waker(&mut inner.read_waker, context.waker());
            return Poll::Pending;
        };
        let amount = buffer.remaining().min(front.bytes.len() - front.offset);
        buffer.put_slice(&front.bytes[front.offset..front.offset + amount]);
        front.offset += amount;
        let frame_finished = front.offset == front.bytes.len();
        if frame_finished {
            inner.inbound.pop_front();
            if !inner.remote_fin {
                inner.consumed_frames = inner
                    .consumed_frames
                    .checked_add(1)
                    .expect("consumption cannot exceed configured frame window");
            }
        }
        inner.inbound_len -= amount;
        if !inner.remote_fin {
            inner.consumed_bytes = inner
                .consumed_bytes
                .checked_add(u32::try_from(amount).expect("frame sizes are u32-bounded"))
                .expect("consumption cannot exceed configured byte window");
        }
        drop(inner);
        self.notify();
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for Channel {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut inner = lock(&self.state.inner);
        if let Some(error) = &inner.terminal {
            return Poll::Ready(Err(channel_io_error(error)));
        }
        if inner.local_fin_requested {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "channel write half is closed",
            )));
        }
        if bytes.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let available = self.buffer_limit.saturating_sub(inner.outbound_len);
        if available == 0 {
            set_waker(&mut inner.write_waker, context.waker());
            return Poll::Pending;
        }
        let amount = available.min(bytes.len());
        inner.outbound.push_back(Chunk {
            bytes: bytes[..amount].to_vec(),
            offset: 0,
        });
        inner.outbound_len += amount;
        drop(inner);
        self.notify();
        Poll::Ready(Ok(amount))
    }

    fn poll_flush(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut inner = lock(&self.state.inner);
        if let Some(error) = &inner.terminal {
            return Poll::Ready(Err(channel_io_error(error)));
        }
        if inner.outbound_len == 0 && inner.outbound_in_flight == 0 {
            Poll::Ready(Ok(()))
        } else {
            set_waker(&mut inner.write_waker, context.waker());
            Poll::Pending
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut inner = lock(&self.state.inner);
        if let Some(error) = &inner.terminal {
            return Poll::Ready(Err(channel_io_error(error)));
        }
        inner.local_fin_requested = true;
        if inner.fin_sent && !inner.fin_in_flight {
            return Poll::Ready(Ok(()));
        }
        set_waker(&mut inner.write_waker, context.waker());
        drop(inner);
        self.notify();
        Poll::Pending
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        let mut inner = lock(&self.state.inner);
        if inner.terminal.is_none() {
            if !inner.remote_fin {
                inner.consumed_bytes = inner
                    .consumed_bytes
                    .checked_add(u32::try_from(inner.inbound_len).expect("buffer limit fits u32"))
                    .expect("discarded bytes are bounded by the receive window");
                inner.consumed_frames = inner
                    .consumed_frames
                    .checked_add(u32::try_from(inner.inbound.len()).expect("frame window fits u32"))
                    .expect("discarded frames are bounded by the receive window");
            }
            inner.inbound.clear();
            inner.inbound_len = 0;
            inner.discard_inbound = true;
            inner.local_fin_requested = true;
            inner.local_close_requested = true;
            inner.wake_all();
        }
        drop(inner);
        self.notify();
    }
}

/// One partially consumed inbound or outbound byte segment.
#[derive(Debug)]
pub(crate) struct Chunk {
    pub(crate) bytes: Vec<u8>,
    pub(crate) offset: usize,
}

/// Flow-control and close-handshake state for one logical channel.
#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)] // Independent read/write wire states are intentional.
pub(crate) struct ChannelInner {
    pub(crate) inbound: VecDeque<Chunk>,
    pub(crate) inbound_len: usize,
    pub(crate) outbound: VecDeque<Chunk>,
    pub(crate) outbound_len: usize,
    pub(crate) outbound_in_flight: usize,
    pub(crate) send_bytes: u32,
    pub(crate) send_frames: u32,
    pub(crate) send_bytes_max: u32,
    pub(crate) send_frames_max: u32,
    pub(crate) send_frame_size: u32,
    pub(crate) receive_bytes: u32,
    pub(crate) receive_frames: u32,
    pub(crate) consumed_bytes: u32,
    pub(crate) consumed_frames: u32,
    pub(crate) discard_inbound: bool,
    pub(crate) window_in_flight: bool,
    pub(crate) local_fin_requested: bool,
    pub(crate) fin_in_flight: bool,
    pub(crate) fin_sent: bool,
    pub(crate) remote_fin: bool,
    pub(crate) local_close_requested: bool,
    pub(crate) closed_in_flight: bool,
    pub(crate) closed_sent: bool,
    pub(crate) remote_closed: bool,
    pub(crate) reset_pending: Option<Vec<u8>>,
    pub(crate) reset_in_flight: bool,
    pub(crate) terminal: Option<Error>,
    pub(crate) read_waker: Option<Waker>,
    pub(crate) write_waker: Option<Waker>,
    pub(crate) close_waker: Option<Waker>,
}

impl ChannelInner {
    /// Creates channel state with this side's advertised receive windows.
    pub(crate) const fn new(config: &Config) -> Self {
        Self {
            inbound: VecDeque::new(),
            inbound_len: 0,
            outbound: VecDeque::new(),
            outbound_len: 0,
            outbound_in_flight: 0,
            send_bytes: 0,
            send_frames: 0,
            send_bytes_max: 0,
            send_frames_max: 0,
            send_frame_size: 0,
            receive_bytes: config.initial_byte_window,
            receive_frames: config.initial_frame_window,
            consumed_bytes: 0,
            consumed_frames: 0,
            discard_inbound: false,
            window_in_flight: false,
            local_fin_requested: false,
            fin_in_flight: false,
            fin_sent: false,
            remote_fin: false,
            local_close_requested: false,
            closed_in_flight: false,
            closed_sent: false,
            remote_closed: false,
            reset_pending: None,
            reset_in_flight: false,
            terminal: None,
            read_waker: None,
            write_waker: None,
            close_waker: None,
        }
    }

    /// Wakes all application tasks waiting on this channel.
    pub(crate) fn wake_all(&mut self) {
        if let Some(waker) = self.read_waker.take() {
            waker.wake();
        }
        if let Some(waker) = self.write_waker.take() {
            waker.wake();
        }
        if let Some(waker) = self.close_waker.take() {
            waker.wake();
        }
    }
}

/// Shared synchronization wrapper for one channel state machine.
#[derive(Debug)]
pub(crate) struct ChannelState {
    pub(crate) id: u32,
    pub(crate) inner: Mutex<ChannelInner>,
}

/// Resets a pending open if its future is cancelled before peer resolution.
#[derive(Debug)]
struct OpenCancellation {
    state: Arc<ChannelState>,
    core: Weak<Core>,
    armed: bool,
}

impl OpenCancellation {
    /// Prevents cancellation cleanup after peer resolution.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for OpenCancellation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut channel = lock(&self.state.inner);
        if channel.terminal.is_some() || channel.reset_pending.is_some() || channel.reset_in_flight
        {
            return;
        }
        channel.reset_pending = Some(b"open cancelled".to_vec());
        drop(channel);
        if let Some(core) = self.core.upgrade() {
            core.notify.notify_one();
        }
    }
}

/// Peer-advertised windows retained until an incoming request is resolved.
#[derive(Debug)]
pub(crate) struct PendingIncoming {
    pub(crate) byte_window: u32,
    pub(crate) frame_window: u32,
    pub(crate) max_frame_size: u32,
}

/// Connection-wide channel registry, scheduler queue, and terminal state.
#[derive(Debug)]
pub(crate) struct CoreInner {
    pub(crate) role: Role,
    pub(crate) config: Config,
    pub(crate) next_id: u32,
    pub(crate) last_peer_id: u32,
    pub(crate) channels: BTreeMap<u32, Arc<ChannelState>>,
    pub(crate) pending_outgoing: HashMap<u32, oneshot::Sender<Result<Channel>>>,
    pub(crate) pending_incoming: HashMap<u32, PendingIncoming>,
    pub(crate) controls: VecDeque<Frame>,
    pub(crate) tombstones: BTreeSet<u32>,
    pub(crate) tombstone_order: VecDeque<u32>,
    pub(crate) incoming_tx: Option<mpsc::Sender<IncomingRequest>>,
    pub(crate) terminal: Option<Error>,
}

impl CoreInner {
    /// Counts active and unresolved incoming channels against the limit.
    pub(crate) fn channel_count(&self) -> usize {
        self.channels.len() + self.pending_incoming.len()
    }

    /// Adds a frame without exceeding the bounded control queue.
    pub(crate) fn enqueue_control(&mut self, frame: Frame) -> Result<()> {
        if self.controls.len() >= self.config.control_queue_capacity {
            return Err(Error::ResourceExhausted.report());
        }
        self.controls.push_back(frame);
        Ok(())
    }

    /// Retains a bounded tombstone for late frames on a closed channel.
    pub(crate) fn remember_closed(&mut self, id: u32) {
        if self.tombstones.insert(id) {
            self.tombstone_order.push_back(id);
        }
        let limit = self.config.closed_channel_capacity;
        while self.tombstone_order.len() > limit {
            if let Some(expired) = self.tombstone_order.pop_front() {
                self.tombstones.remove(&expired);
            }
        }
    }
}

/// Shared connection state and driver notification primitive.
#[derive(Debug)]
pub(crate) struct Core {
    pub(crate) inner: Mutex<CoreInner>,
    pub(crate) notify: Notify,
}

impl Core {
    /// Fails the connection and wakes every pending operation.
    pub(crate) fn fail(&self, error: &Error) {
        let (responses, channels) = {
            let mut inner = lock(&self.inner);
            if inner.terminal.is_some() {
                return;
            }
            inner.terminal = Some(error.clone());
            inner.incoming_tx = None;
            inner.controls.clear();
            inner.pending_incoming.clear();
            let responses = inner.pending_outgoing.drain().collect::<Vec<_>>();
            let channels = inner.channels.values().cloned().collect::<Vec<_>>();
            inner.channels.clear();
            (responses, channels)
        };

        for (channel_id, response) in responses {
            if response.send(Err(terminal_report(error))).is_err() {
                debug!(channel_id, "pending channel opener was already dropped");
            }
        }
        for state in channels {
            let mut channel = lock(&state.inner);
            channel.terminal = Some(error.clone());
            channel.inbound.clear();
            channel.inbound_len = 0;
            channel.outbound.clear();
            channel.outbound_len = 0;
            channel.wake_all();
        }
        self.notify.notify_waiters();
    }
}

/// Queues a peer-visible rejection while preserving atomic request state.
pub(crate) fn reject_request(core: &Arc<Core>, id: u32, reason: &[u8]) -> Result<()> {
    {
        let mut inner = lock(&core.inner);
        if let Some(error) = &inner.terminal {
            return Err(terminal_report(error));
        }
        if reason.len() > inner.config.max_reason_size {
            return Err(Error::InvalidInput.report());
        }
        if !inner.pending_incoming.contains_key(&id) {
            return Err(Error::ConnectionClosed.report());
        }
        inner.enqueue_control(Frame::Reject {
            id,
            reason: reason.to_vec(),
        })?;
        inner.pending_incoming.remove(&id);
        inner.remember_closed(id);
    }
    core.notify.notify_one();
    Ok(())
}

/// Maps a channel terminal state to Tokio's I/O error model.
fn channel_io_error(error: &Error) -> io::Error {
    let kind = match error {
        Error::Reset(_) => io::ErrorKind::ConnectionReset,
        Error::ConnectionClosed | Error::Io => io::ErrorKind::BrokenPipe,
        _ => io::ErrorKind::InvalidData,
    };
    io::Error::new(kind, error.to_string())
}

/// Replaces a stored waker only when the task has changed.
fn set_waker(slot: &mut Option<Waker>, waker: &Waker) {
    if slot.as_ref().is_none_or(|old| !old.will_wake(waker)) {
        *slot = Some(waker.clone());
    }
}

/// Acquires state while recovering data from a poisoned mutex.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(error) => {
            warn!("recovering multiplex state after mutex poison");
            error.into_inner()
        }
    }
}
