//! Transport lifecycle, frame dispatch, and fair channel scheduling.
//!
//! [`Driver`] owns transport progress and coordinates channel state with the
//! connection module's bounded queues using validated protocol frames.

use std::sync::Arc;

use reportify::ErrorExt as _;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tracing::debug;

use crate::Error;
use crate::Result;
use crate::connection::Channel;
use crate::connection::ChannelState;
use crate::connection::Chunk;
use crate::connection::Core;
use crate::connection::CoreInner;
use crate::connection::IncomingRequest;
use crate::connection::PendingIncoming;
use crate::connection::lock;
use crate::protocol::Frame;
use crate::protocol::probe_handshake;
use crate::protocol::read_frame;
use crate::protocol::write_frame;
use crate::terminal_report;

/// The connection driver. It must be polled for channels to make progress.
///
/// Dropping it fails all local operations and closes its transport. The
/// transport must keep non-probe bytes from that closed generation out of any
/// later connection, as described by the crate-level transport contract.
#[derive(Debug)]
pub struct Driver<T> {
    pub(crate) io: Option<T>,
    pub(crate) core: Arc<Core>,
}

impl<T> Driver<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    /// Exchanges synchronization probes and drives frames until either side
    /// closes.
    ///
    /// # Errors
    ///
    /// Returns an I/O or protocol error when the transport closes or the peer
    /// sends an invalid frame. All pending channel operations are woken first.
    #[tracing::instrument(
        name = "tascarrel_mux.driver.run",
        level = "debug",
        skip(self),
        fields(
            role = tracing::field::Empty,
            probe_interval = tracing::field::Empty,
        ),
        err(level = "debug")
    )]
    pub async fn run(mut self) -> Result<()> {
        let io = self
            .io
            .take()
            .ok_or_else(|| Error::ConnectionClosed.report())?;
        let core = Arc::clone(&self.core);
        let (mut reader, mut writer) = tokio::io::split(io);
        let (role, probe_interval) = {
            let inner = lock(&core.inner);
            (inner.role, inner.config.probe_interval)
        };
        tracing::Span::current().record("role", tracing::field::debug(role));
        tracing::Span::current().record("probe_interval", tracing::field::debug(probe_interval));
        let synchronization = probe_handshake(&mut reader, &mut writer, probe_interval).await;
        if let Err(report) = synchronization {
            core.fail(report.error());
            return Err(report);
        }

        let result = tokio::select! {
            result = reader_loop(&mut reader, &core) => result,
            result = writer_loop(&mut writer, &core) => result,
        };
        let report = match result {
            Ok(()) => Error::ConnectionClosed.report(),
            Err(report) => report,
        };
        core.fail(report.error());
        Err(report)
    }
}

impl<T> Drop for Driver<T> {
    fn drop(&mut self) {
        self.core.fail(&Error::ConnectionClosed);
    }
}

/// Continuously reads validated frames and applies them to connection state.
#[tracing::instrument(
    name = "tascarrel_mux.driver.read",
    level = "trace",
    skip_all,
    err(level = "debug")
)]
async fn reader_loop<R: AsyncRead + Unpin>(reader: &mut R, core: &Arc<Core>) -> Result<()> {
    loop {
        let config = lock(&core.inner).config.clone();
        let frame = read_frame(reader, &config).await?;
        process_frame(core, frame)?;
    }
}

/// Dispatches one validated frame to its state transition.
fn process_frame(core: &Arc<Core>, frame: Frame) -> Result<()> {
    match frame {
        Frame::Open {
            id,
            byte_window,
            frame_window,
            max_frame_size,
            endpoint,
        } => receive_open(
            core,
            id,
            byte_window,
            frame_window,
            max_frame_size,
            endpoint,
        ),
        Frame::Accept {
            id,
            byte_window,
            frame_window,
            max_frame_size,
        } => receive_accept(core, id, byte_window, frame_window, max_frame_size),
        Frame::Reject { id, reason } => receive_reject(core, id, &reason),
        Frame::Data { id, bytes } => receive_data(core, id, bytes),
        Frame::Window { id, bytes, frames } => receive_window(core, id, bytes, frames),
        Frame::Fin { id } => receive_fin(core, id),
        Frame::Reset { id, reason } => receive_reset(core, id, &reason),
        Frame::Closed { id } => receive_closed(core, id),
    }
}

/// Registers or rejects a peer channel-open request.
fn receive_open(
    core: &Arc<Core>,
    id: u32,
    byte_window: u32,
    frame_window: u32,
    max_frame_size: u32,
    endpoint: String,
) -> Result<()> {
    let mut inner = lock(&core.inner);
    if inner.role.owns_id(id) || id <= inner.last_peer_id {
        return Err(Error::Protocol("invalid or reused peer channel identifier".into()).report());
    }
    inner.last_peer_id = id;
    if endpoint.is_empty() {
        return Err(Error::Protocol("empty channel endpoint".into()).report());
    }
    if inner.channel_count() >= inner.config.max_channels {
        inner.enqueue_control(Frame::Reject {
            id,
            reason: b"channel limit reached".to_vec(),
        })?;
        inner.remember_closed(id);
        drop(inner);
        core.notify.notify_one();
        return Ok(());
    }
    let Some(sender) = inner.incoming_tx.clone() else {
        return Err(Error::ConnectionClosed.report());
    };
    inner.pending_incoming.insert(
        id,
        PendingIncoming {
            byte_window,
            frame_window,
            max_frame_size,
        },
    );
    let request = IncomingRequest {
        id,
        endpoint,
        core: Arc::downgrade(core),
        resolved: false,
    };
    if let Err(error) = sender.try_send(request) {
        // A failed send returns the request. Resolve it before it is dropped
        // while the core lock is held, avoiding a recursive auto-reject.
        let mut request = error.into_inner();
        request.resolved = true;
        inner.pending_incoming.remove(&id);
        inner.enqueue_control(Frame::Reject {
            id,
            reason: b"incoming queue full".to_vec(),
        })?;
        inner.remember_closed(id);
        drop(inner);
        core.notify.notify_one();
    }
    Ok(())
}

/// Resolves a locally opened channel with peer-advertised windows.
fn receive_accept(
    core: &Arc<Core>,
    id: u32,
    bytes: u32,
    frames: u32,
    max_frame_size: u32,
) -> Result<()> {
    let (response, state, buffer_limit) = {
        let mut inner = lock(&core.inner);
        if !inner.role.owns_id(id) {
            return Err(Error::Protocol("ACCEPT for peer-owned channel".into()).report());
        }
        let Some(response) = inner.pending_outgoing.remove(&id) else {
            if inner.tombstones.contains(&id) {
                return Ok(());
            }
            return Err(Error::Protocol("unexpected ACCEPT".into()).report());
        };
        let state = inner
            .channels
            .get(&id)
            .cloned()
            .ok_or_else(|| Error::Protocol("ACCEPT for unknown channel".into()).report())?;
        let limit = usize::try_from(inner.config.initial_byte_window)
            .expect("u32 always fits supported targets");
        (response, state, limit)
    };
    {
        let mut channel = lock(&state.inner);
        channel.send_bytes = bytes;
        channel.send_frames = frames;
        channel.send_bytes_max = bytes;
        channel.send_frames_max = frames;
        channel.send_frame_size = max_frame_size;
    }
    let channel = Channel {
        state: Arc::clone(&state),
        core: Arc::downgrade(core),
        buffer_limit,
    };
    if response.send(Ok(channel)).is_err() {
        let mut channel = lock(&state.inner);
        channel.reset_pending = Some(b"open cancelled".to_vec());
        drop(channel);
        core.notify.notify_one();
    }
    Ok(())
}

/// Resolves a locally opened channel as rejected.
fn receive_reject(core: &Arc<Core>, id: u32, reason: &[u8]) -> Result<()> {
    let (response, state) = {
        let mut inner = lock(&core.inner);
        if !inner.role.owns_id(id) {
            return Err(Error::Protocol("REJECT for peer-owned channel".into()).report());
        }
        let Some(response) = inner.pending_outgoing.remove(&id) else {
            if inner.tombstones.contains(&id) {
                return Ok(());
            }
            return Err(Error::Protocol("unexpected REJECT".into()).report());
        };
        let state = inner.channels.remove(&id);
        inner.remember_closed(id);
        (response, state)
    };
    let reason = String::from_utf8_lossy(reason).into_owned();
    if let Some(state) = state {
        let mut channel = lock(&state.inner);
        channel.terminal = Some(Error::Rejected(reason.clone()));
        channel.wake_all();
    }
    if response
        .send(Err(Error::Rejected(reason).report()))
        .is_err()
    {
        debug!(
            channel_id = id,
            "rejected channel opener was already dropped"
        );
    }
    Ok(())
}

/// Queues peer data after enforcing byte and frame credit.
fn receive_data(core: &Arc<Core>, id: u32, bytes: Vec<u8>) -> Result<()> {
    let state = {
        let inner = lock(&core.inner);
        if inner.pending_outgoing.contains_key(&id) {
            return Err(Error::Protocol("DATA before channel acceptance".into()).report());
        }
        match inner.channels.get(&id).cloned() {
            Some(state) => Some(state),
            None if inner.tombstones.contains(&id) => None,
            None => return Err(Error::Protocol("DATA for unknown channel".into()).report()),
        }
    };
    let Some(state) = state else {
        return Ok(());
    };
    let length = u32::try_from(bytes.len()).expect("wire DATA is u32-bounded");
    let mut channel = lock(&state.inner);
    if channel.remote_fin || channel.terminal.is_some() {
        return Err(Error::Protocol("DATA after channel close".into()).report());
    }
    channel.receive_bytes = channel
        .receive_bytes
        .checked_sub(length)
        .ok_or_else(|| Error::Protocol("peer exceeded channel byte credit".into()).report())?;
    channel.receive_frames = channel
        .receive_frames
        .checked_sub(1)
        .ok_or_else(|| Error::Protocol("peer exceeded channel frame credit".into()).report())?;
    if channel.discard_inbound {
        channel.consumed_bytes = channel
            .consumed_bytes
            .checked_add(length)
            .expect("discarded bytes are bounded by the receive window");
        channel.consumed_frames = channel
            .consumed_frames
            .checked_add(1)
            .expect("discarded frames are bounded by the receive window");
        drop(channel);
        core.notify.notify_one();
        return Ok(());
    }
    channel.inbound_len = channel
        .inbound_len
        .checked_add(bytes.len())
        .ok_or_else(|| Error::Protocol("channel receive buffer overflow".into()).report())?;
    channel.inbound.push_back(Chunk { bytes, offset: 0 });
    if let Some(waker) = channel.read_waker.take() {
        waker.wake();
    }
    Ok(())
}

/// Applies returned send credit and wakes the channel writer.
fn receive_window(core: &Arc<Core>, id: u32, bytes: u32, frames: u32) -> Result<()> {
    let state = {
        let inner = lock(&core.inner);
        match inner.channels.get(&id).cloned() {
            Some(state) => Some(state),
            None if inner.tombstones.contains(&id) => None,
            None => return Err(Error::Protocol("WINDOW for unknown channel".into()).report()),
        }
    };
    let Some(state) = state else {
        return Ok(());
    };
    let mut channel = lock(&state.inner);
    channel.send_bytes = channel
        .send_bytes
        .checked_add(bytes)
        .filter(|value| *value <= channel.send_bytes_max)
        .ok_or_else(|| Error::Protocol("invalid channel byte credit update".into()).report())?;
    channel.send_frames = channel
        .send_frames
        .checked_add(frames)
        .filter(|value| *value <= channel.send_frames_max)
        .ok_or_else(|| Error::Protocol("invalid channel frame credit update".into()).report())?;
    drop(channel);
    core.notify.notify_one();
    Ok(())
}

/// Records the peer's graceful write-side close.
fn receive_fin(core: &Arc<Core>, id: u32) -> Result<()> {
    let state = {
        let inner = lock(&core.inner);
        match inner.channels.get(&id).cloned() {
            Some(state) => Some(state),
            None if inner.tombstones.contains(&id) => None,
            None => return Err(Error::Protocol("FIN for unknown channel".into()).report()),
        }
    };
    let Some(state) = state else {
        return Ok(());
    };
    let mut channel = lock(&state.inner);
    if channel.remote_fin {
        return Err(Error::Protocol("duplicate FIN".into()).report());
    }
    channel.remote_fin = true;
    // No more DATA can arrive from this peer, so any credit not already on
    // the wire is unnecessary. An in-flight WINDOW is ordered before CLOSED.
    channel.consumed_bytes = 0;
    channel.consumed_frames = 0;
    if let Some(waker) = channel.read_waker.take() {
        waker.wake();
    }
    drop(channel);
    core.notify.notify_one();
    Ok(())
}

/// Records the peer's acknowledgement of a fully closed channel.
fn receive_closed(core: &Arc<Core>, id: u32) -> Result<()> {
    let state = {
        let inner = lock(&core.inner);
        match inner.channels.get(&id).cloned() {
            Some(state) => Some(state),
            None if inner.tombstones.contains(&id) => None,
            None => return Err(Error::Protocol("CLOSED for unknown channel".into()).report()),
        }
    };
    let Some(state) = state else {
        return Ok(());
    };
    let mut channel = lock(&state.inner);
    if !channel.fin_sent || !channel.remote_fin || channel.remote_closed {
        return Err(Error::Protocol("unexpected or duplicate CLOSED".into()).report());
    }
    channel.remote_closed = true;
    if let Some(waker) = channel.close_waker.take() {
        waker.wake();
    }
    let remove = channel.closed_sent;
    drop(channel);
    if remove {
        let mut inner = lock(&core.inner);
        inner.channels.remove(&id);
        inner.remember_closed(id);
    }
    core.notify.notify_one();
    Ok(())
}

/// Terminates channel state after a peer reset.
fn receive_reset(core: &Arc<Core>, id: u32, reason: &[u8]) -> Result<()> {
    let (state, pending_open) = {
        let mut inner = lock(&core.inner);
        let pending_open = inner.pending_outgoing.remove(&id);
        let was_pending_incoming = inner.pending_incoming.remove(&id).is_some();
        let state = inner.channels.remove(&id);
        if state.is_none()
            && pending_open.is_none()
            && !was_pending_incoming
            && !inner.tombstones.contains(&id)
        {
            return Err(Error::Protocol("RESET for unknown channel".into()).report());
        }
        inner.remember_closed(id);
        (state, pending_open)
    };
    let reason = String::from_utf8_lossy(reason).into_owned();
    if let Some(state) = state {
        let mut channel = lock(&state.inner);
        channel.terminal = Some(Error::Reset(reason.clone()));
        channel.inbound.clear();
        channel.inbound_len = 0;
        channel.outbound.clear();
        channel.outbound_len = 0;
        channel.wake_all();
    }
    if let Some(response) = pending_open
        && response.send(Err(Error::Reset(reason).report())).is_err()
    {
        debug!(channel_id = id, "reset channel opener was already dropped");
    }
    Ok(())
}

/// State mutation committed after a selected frame reaches the transport.
#[derive(Debug)]
enum Completion {
    None,
    Data {
        state: Arc<ChannelState>,
        bytes: usize,
    },
    Window {
        state: Arc<ChannelState>,
        bytes: u32,
        frames: u32,
    },
    Fin {
        state: Arc<ChannelState>,
    },
    Reset {
        id: u32,
        state: Arc<ChannelState>,
    },
    Closed {
        id: u32,
        state: Arc<ChannelState>,
    },
}

/// A wire frame paired with its post-write state transition.
#[derive(Debug)]
struct Selected {
    frame: Frame,
    completion: Completion,
}

/// Continuously schedules, writes, and commits ready frames.
#[tracing::instrument(
    name = "tascarrel_mux.driver.write",
    level = "trace",
    skip_all,
    err(level = "debug")
)]
async fn writer_loop<W: AsyncWrite + Unpin>(writer: &mut W, core: &Arc<Core>) -> Result<()> {
    let mut data_cursor = 0_u32;
    let mut control_cursor = 0_u32;
    let mut control_count = 0_usize;
    loop {
        let notified = core.notify.notified();
        let selected = select_frame(core, &mut data_cursor, &mut control_cursor, control_count)?;
        let Some(selected) = selected else {
            if let Some(error) = &lock(&core.inner).terminal {
                return Err(terminal_report(error));
            }
            notified.await;
            continue;
        };
        let is_data = matches!(selected.frame, Frame::Data { .. });
        write_frame(writer, &selected.frame).await?;
        complete_frame(core, selected.completion);
        if is_data {
            control_count = 0;
        } else {
            control_count = control_count.saturating_add(1);
        }
    }
}

/// Selects the next frame while balancing control and data traffic.
fn select_frame(
    core: &Arc<Core>,
    data_cursor: &mut u32,
    control_cursor: &mut u32,
    control_count: usize,
) -> Result<Option<Selected>> {
    let mut inner = lock(&core.inner);
    if let Some(error) = &inner.terminal {
        return Err(terminal_report(error));
    }
    let force_data = control_count >= inner.config.control_burst;
    if !force_data {
        if let Some(frame) = inner.controls.pop_front() {
            return Ok(Some(Selected {
                frame,
                completion: Completion::None,
            }));
        }
        if let Some(selected) = select_channel_control(&inner, control_cursor) {
            return Ok(Some(selected));
        }
    }
    if let Some(selected) = select_channel_data(&inner, data_cursor) {
        return Ok(Some(selected));
    }
    if force_data {
        if let Some(frame) = inner.controls.pop_front() {
            return Ok(Some(Selected {
                frame,
                completion: Completion::None,
            }));
        }
        if let Some(selected) = select_channel_control(&inner, control_cursor) {
            return Ok(Some(selected));
        }
    }
    Ok(None)
}

/// Iterates channels after a cursor and wraps once for round-robin fairness.
fn ordered_states(
    inner: &CoreInner,
    after: u32,
) -> impl Iterator<Item = (&u32, &Arc<ChannelState>)> {
    inner
        .channels
        .range((std::ops::Bound::Excluded(after), std::ops::Bound::Unbounded))
        .chain(inner.channels.range(..=after))
}

/// Selects the next reset, window, FIN, or CLOSED frame.
fn select_channel_control(inner: &CoreInner, cursor: &mut u32) -> Option<Selected> {
    for (&id, state) in ordered_states(inner, *cursor) {
        let mut channel = lock(&state.inner);
        if let Some(reason) = channel.reset_pending.take() {
            channel.reset_in_flight = true;
            *cursor = id;
            return Some(Selected {
                frame: Frame::Reset { id, reason },
                completion: Completion::Reset {
                    id,
                    state: Arc::clone(state),
                },
            });
        }
        if !channel.remote_fin
            && !channel.window_in_flight
            && (channel.consumed_bytes != 0 || channel.consumed_frames != 0)
        {
            let bytes = std::mem::take(&mut channel.consumed_bytes);
            let frames = std::mem::take(&mut channel.consumed_frames);
            channel.window_in_flight = true;
            *cursor = id;
            return Some(Selected {
                frame: Frame::Window { id, bytes, frames },
                completion: Completion::Window {
                    state: Arc::clone(state),
                    bytes,
                    frames,
                },
            });
        }
        if channel.local_fin_requested
            && !channel.fin_sent
            && !channel.fin_in_flight
            && channel.outbound_len == 0
            && channel.outbound_in_flight == 0
        {
            channel.fin_in_flight = true;
            // Commit the wire state before yielding to the async write. The
            // peer can receive FIN and return CLOSED before this writer is
            // polled again to complete the frame. Keep `fin_in_flight` set so
            // `poll_shutdown` still waits until the write itself completes.
            channel.fin_sent = true;
            *cursor = id;
            return Some(Selected {
                frame: Frame::Fin { id },
                completion: Completion::Fin {
                    state: Arc::clone(state),
                },
            });
        }
        if channel.fin_sent
            && channel.remote_fin
            && !channel.fin_in_flight
            && !channel.window_in_flight
            && channel.local_close_requested
            && !channel.closed_sent
            && !channel.closed_in_flight
        {
            channel.closed_in_flight = true;
            *cursor = id;
            return Some(Selected {
                frame: Frame::Closed { id },
                completion: Completion::Closed {
                    id,
                    state: Arc::clone(state),
                },
            });
        }
    }
    None
}

/// Selects one bounded DATA frame from the next writable channel.
fn select_channel_data(inner: &CoreInner, cursor: &mut u32) -> Option<Selected> {
    for (&id, state) in ordered_states(inner, *cursor) {
        let mut channel = lock(&state.inner);
        if channel.reset_pending.is_some()
            || channel.reset_in_flight
            || channel.send_bytes == 0
            || channel.send_frames == 0
            || channel.outbound_len == 0
        {
            continue;
        }
        let amount = usize::try_from(channel.send_bytes)
            .unwrap_or(usize::MAX)
            .min(usize::try_from(channel.send_frame_size).expect("u32 fits usize"))
            .min(channel.outbound_len);
        let mut bytes = Vec::with_capacity(amount);
        while bytes.len() < amount {
            let front = channel
                .outbound
                .front_mut()
                .expect("outbound length tracks chunks");
            let take = (amount - bytes.len()).min(front.bytes.len() - front.offset);
            bytes.extend_from_slice(&front.bytes[front.offset..front.offset + take]);
            front.offset += take;
            if front.offset == front.bytes.len() {
                channel.outbound.pop_front();
            }
        }
        channel.outbound_len -= amount;
        channel.outbound_in_flight += amount;
        channel.send_bytes -= u32::try_from(amount).expect("frame size is u32-bounded");
        channel.send_frames -= 1;
        *cursor = id;
        return Some(Selected {
            frame: Frame::Data { id, bytes },
            completion: Completion::Data {
                state: Arc::clone(state),
                bytes: amount,
            },
        });
    }
    None
}

/// Commits state associated with a successfully written frame.
fn complete_frame(core: &Arc<Core>, completion: Completion) {
    match completion {
        Completion::None => {}
        Completion::Data { state, bytes } => {
            let mut channel = lock(&state.inner);
            channel.outbound_in_flight -= bytes;
            if let Some(waker) = channel.write_waker.take() {
                waker.wake();
            }
        }
        Completion::Window {
            state,
            bytes,
            frames,
        } => {
            let config = lock(&core.inner).config.clone();
            let mut channel = lock(&state.inner);
            channel.window_in_flight = false;
            channel.receive_bytes = channel
                .receive_bytes
                .checked_add(bytes)
                .filter(|value| *value <= config.initial_byte_window)
                .expect("consumed byte credit is bounded by the receive window");
            channel.receive_frames = channel
                .receive_frames
                .checked_add(frames)
                .filter(|value| *value <= config.initial_frame_window)
                .expect("consumed frame credit is bounded by the receive window");
        }
        Completion::Fin { state } => {
            let mut channel = lock(&state.inner);
            channel.fin_in_flight = false;
            if let Some(waker) = channel.write_waker.take() {
                waker.wake();
            }
        }
        Completion::Reset { id, state } => {
            {
                let mut channel = lock(&state.inner);
                channel.reset_in_flight = false;
                channel.terminal = Some(Error::Reset("local channel reset".into()));
                channel.wake_all();
            }
            let mut inner = lock(&core.inner);
            inner.pending_outgoing.remove(&id);
            inner.channels.remove(&id);
            inner.remember_closed(id);
        }
        Completion::Closed { id, state } => {
            let mut channel = lock(&state.inner);
            channel.closed_in_flight = false;
            channel.closed_sent = true;
            if let Some(waker) = channel.close_waker.take() {
                waker.wake();
            }
            let remove = channel.remote_closed;
            drop(channel);
            if remove {
                let mut inner = lock(&core.inner);
                inner.channels.remove(&id);
                inner.remember_closed(id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::io;
    use std::sync::Mutex;
    use std::time::Duration;

    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;
    use tokio::io::DuplexStream;
    use tokio::task::JoinHandle;
    use tokio::time::timeout;

    use super::*;
    use crate::Config;
    use crate::Role;
    use crate::connection::ChannelInner;
    use crate::connection::Incoming;
    use crate::connection::MuxHandle;
    use crate::connection::connect;
    use crate::connection::reject_request;
    use crate::protocol::HEADER_LEN;
    use crate::protocol::KIND_DATA;
    use crate::protocol::KIND_FIN;
    use crate::protocol::KIND_OPEN;
    use crate::protocol::KIND_PROBE;
    use crate::protocol::read_u32;

    const TEST_TIMEOUT: Duration = Duration::from_secs(3);

    struct TestPair {
        client: MuxHandle,
        client_incoming: Incoming,
        server: MuxHandle,
        server_incoming: Incoming,
        client_driver: JoinHandle<Result<()>>,
        server_driver: JoinHandle<Result<()>>,
    }

    impl Drop for TestPair {
        fn drop(&mut self) {
            self.client_driver.abort();
            self.server_driver.abort();
        }
    }

    async fn within<F: Future>(future: F) -> F::Output {
        timeout(TEST_TIMEOUT, future)
            .await
            .expect("operation timed out")
    }

    /// Verifies that one reciprocal probe ends synchronization before frames.
    #[tokio::test]
    async fn reciprocal_probe_precedes_frames_and_ends_synchronization() {
        let (server_io, mut client_io) = tokio::io::duplex(64);
        let mut config = test_config();
        config.probe_interval = Duration::from_secs(1);
        let (driver, handle, _incoming) =
            connect(server_io, Role::Server, config).expect("server setup");
        let driver = tokio::spawn(driver.run());
        let open = tokio::spawn(async move { handle.open("control").await });

        let mut probe = [u8::MAX; 1];
        within(client_io.read_exact(&mut probe))
            .await
            .expect("server probe");
        assert_eq!(probe, [KIND_PROBE]);
        client_io
            .write_all(&[KIND_PROBE])
            .await
            .expect("client probe");

        within(client_io.read_exact(&mut probe))
            .await
            .expect("reciprocal server probe");
        assert_eq!(probe, [KIND_PROBE]);
        let mut header = [u8::MAX; HEADER_LEN];
        within(client_io.read_exact(&mut header))
            .await
            .expect("OPEN header after reciprocal probe");
        assert_eq!(header[0], KIND_OPEN);

        driver.abort();
        assert!(
            driver
                .await
                .expect_err("driver task aborted")
                .is_cancelled()
        );
        assert!(within(open).await.expect("open task").is_err());
    }

    /// Verifies that synchronization tolerates probes dropped before
    /// attachment.
    #[tokio::test]
    async fn initial_probes_may_be_dropped_before_peer_attachment() {
        let (client_io, mut server_io) = tokio::io::duplex(64);
        let client_config = test_config();
        let (client_driver, client, _client_incoming) =
            connect(client_io, Role::Client, client_config).expect("client setup");
        let client_driver = tokio::spawn(client_driver.run());
        let open = tokio::spawn(async move { client.open("late-server").await });

        for _ in 0..3 {
            let mut dropped = [u8::MAX; 1];
            within(server_io.read_exact(&mut dropped))
                .await
                .expect("discarded probe");
            assert_eq!(dropped, [KIND_PROBE]);
        }
        let (server_driver, _server, mut server_incoming) =
            connect(server_io, Role::Server, test_config()).expect("server setup");
        let server_driver = tokio::spawn(server_driver.run());
        let request = within(server_incoming.recv())
            .await
            .expect("retried open request");
        let _accepted = request.accept().expect("accept retried channel");
        let _opened = within(open).await.expect("open task").expect("open result");

        client_driver.abort();
        server_driver.abort();
    }

    /// Verifies that dropping a probing driver wakes pending API operations.
    #[tokio::test]
    async fn dropping_driver_during_probe_wakes_pending_operations() {
        let (io, mut peer) = tokio::io::duplex(64);
        let (driver, handle, mut incoming) =
            connect(io, Role::Client, test_config()).expect("client setup");
        let driver = tokio::spawn(driver.run());
        let mut probe = [u8::MAX; 1];
        within(peer.read_exact(&mut probe))
            .await
            .expect("client probe");
        assert_eq!(probe, [KIND_PROBE]);

        let pending = tokio::spawn(async move { handle.open("pending").await });
        driver.abort();
        assert!(
            driver
                .await
                .expect_err("driver task aborted")
                .is_cancelled()
        );

        assert!(within(pending).await.expect("open task").is_err());
        assert!(within(incoming.recv()).await.is_none());
    }

    fn test_config() -> Config {
        Config {
            initial_byte_window: 64,
            initial_frame_window: 4,
            max_frame_size: 16,
            max_channels: 8,
            max_endpoint_size: 64,
            max_reason_size: 64,
            control_queue_capacity: 48,
            control_burst: 16,
            closed_channel_capacity: 32,
            probe_interval: Duration::from_millis(10),
        }
    }

    fn pair(config: Config) -> TestPair {
        pair_with_configs(config.clone(), config)
    }

    fn pair_with_configs(client_config: Config, server_config: Config) -> TestPair {
        let (client_io, server_io) = tokio::io::duplex(4096);
        let (client_driver, client, client_incoming) =
            connect(client_io, Role::Client, client_config).expect("client setup");
        let (server_driver, server, server_incoming) =
            connect(server_io, Role::Server, server_config).expect("server setup");
        TestPair {
            client,
            client_incoming,
            server,
            server_incoming,
            client_driver: tokio::spawn(client_driver.run()),
            server_driver: tokio::spawn(server_driver.run()),
        }
    }

    async fn open_channel(
        opener: &MuxHandle,
        incoming: &mut Incoming,
        endpoint: &str,
    ) -> (Channel, Channel) {
        let open = opener.open(endpoint);
        let accept = async {
            let request = incoming.recv().await.expect("incoming request");
            assert_eq!(request.endpoint(), endpoint);
            request.accept().expect("accept channel")
        };
        let (open_result, accepted) = within(async { tokio::join!(open, accept) }).await;
        (open_result.expect("open channel"), accepted)
    }

    /// Verifies explicit acceptance, rejection, and automatic rejection of
    /// opens.
    #[tokio::test]
    async fn opens_accepts_rejects_and_auto_rejects() {
        let mut pair = pair(test_config());
        let (first, second) =
            open_channel(&pair.client, &mut pair.server_incoming, "control").await;
        assert_eq!(first.id(), second.id());
        assert_eq!(first.id() % 2, 1);

        let rejected = pair.client.open("nope");
        let reject = async {
            pair.server_incoming
                .recv()
                .await
                .expect("reject request")
                .reject(b"not available")
                .expect("send rejection");
        };
        let (result, ()) = within(async { tokio::join!(rejected, reject) }).await;
        assert!(matches!(
            result,
            Err(report)
                if matches!(report.error(), Error::Rejected(reason) if reason == "not available")
        ));

        let dropped = pair.client.open("drop");
        let drop_request = async {
            drop(pair.server_incoming.recv().await.expect("dropped request"));
        };
        let (result, ()) = within(async { tokio::join!(dropped, drop_request) }).await;
        assert!(matches!(
            result,
            Err(report)
                if matches!(report.error(), Error::Rejected(reason) if reason == "request dropped")
        ));
    }

    /// Verifies that a failed rejection leaves the incoming request resolvable.
    #[tokio::test]
    async fn failed_rejection_keeps_the_request_pending() {
        let (io, _peer) = tokio::io::duplex(64);
        let mut config = test_config();
        config.max_channels = 1;
        let (_driver, handle, mut incoming) =
            connect(io, Role::Server, config).expect("connection setup");
        receive_open(&handle.core, 1, 64, 4, 8, "queued".to_owned()).expect("receive OPEN");
        let request = within(incoming.recv()).await.expect("incoming request");

        {
            let mut inner = lock(&handle.core.inner);
            while inner.controls.len() < inner.config.control_queue_capacity {
                inner.controls.push_back(Frame::Closed { id: 1 });
            }
        }
        let report = reject_request(&handle.core, 1, b"busy").expect_err("control queue is full");
        assert_eq!(report.error(), &Error::ResourceExhausted);
        assert!(lock(&handle.core.inner).pending_incoming.contains_key(&1));

        lock(&handle.core.inner).controls.clear();
        request.reject(b"busy").expect("retry rejection");
        assert!(!lock(&handle.core.inner).pending_incoming.contains_key(&1));
    }

    /// Verifies bidirectional byte streams across protocol frame boundaries.
    #[tokio::test]
    async fn transfers_bidirectionally_and_preserves_boundaries_as_bytes() {
        let mut pair = pair(test_config());
        let (mut client, mut server) =
            open_channel(&pair.client, &mut pair.server_incoming, "control").await;
        client.write_all(b"one").await.expect("client write one");
        client.write_all(b"-two").await.expect("client write two");
        let mut received = [0_u8; 7];
        within(server.read_exact(&mut received))
            .await
            .expect("server read");
        assert_eq!(&received, b"one-two");

        server.write_all(b"reply").await.expect("server write");
        let mut reply = [0_u8; 5];
        within(client.read_exact(&mut reply))
            .await
            .expect("client read");
        assert_eq!(&reply, b"reply");
    }

    /// Verifies that byte credit resumes only after peer consumption.
    #[tokio::test]
    async fn byte_flow_control_stalls_and_resumes_after_consumption() {
        let mut config = test_config();
        config.initial_byte_window = 32;
        config.initial_frame_window = 8;
        let mut pair = pair(config);
        let (mut sender, mut receiver) =
            open_channel(&pair.client, &mut pair.server_incoming, "flow").await;
        let write = tokio::spawn(async move {
            sender.write_all(&[7_u8; 96]).await?;
            sender.flush().await?;
            Ok::<Channel, io::Error>(sender)
        });

        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!write.is_finished(), "writer exceeded the byte window");
        let mut bytes = [0_u8; 96];
        within(receiver.read_exact(&mut bytes))
            .await
            .expect("consume flow-controlled bytes");
        let _sender = within(write)
            .await
            .expect("writer task")
            .expect("writer I/O");
        assert_eq!(bytes, [7_u8; 96]);
    }

    /// Verifies that frame credit is enforced independently of byte credit.
    #[tokio::test]
    async fn frame_flow_control_is_independent_of_byte_credit() {
        let mut config = test_config();
        config.initial_byte_window = 64;
        config.initial_frame_window = 1;
        config.max_frame_size = 8;
        let mut pair = pair(config);
        let (mut sender, mut receiver) =
            open_channel(&pair.client, &mut pair.server_incoming, "frames").await;
        let write = tokio::spawn(async move {
            sender.write_all(&[9_u8; 80]).await?;
            Ok::<Channel, io::Error>(sender)
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!write.is_finished(), "writer exceeded the frame window");

        let mut bytes = [0_u8; 80];
        within(receiver.read_exact(&mut bytes))
            .await
            .expect("read frame-controlled bytes");
        let _sender = within(write)
            .await
            .expect("writer task")
            .expect("writer I/O");
    }

    /// Verifies that frame-size limits are negotiated independently by
    /// direction.
    #[tokio::test]
    async fn asymmetric_max_frame_sizes_are_negotiated_per_direction() {
        let mut client_config = test_config();
        client_config.max_frame_size = 16;
        let mut server_config = test_config();
        server_config.max_frame_size = 4;
        let mut pair = pair_with_configs(client_config, server_config);
        let (mut client, mut server) =
            open_channel(&pair.client, &mut pair.server_incoming, "asymmetric").await;

        client.write_all(&[1_u8; 32]).await.expect("client write");
        let mut from_client = [0_u8; 32];
        within(server.read_exact(&mut from_client))
            .await
            .expect("server accepts negotiated four-byte frames");

        server.write_all(&[2_u8; 32]).await.expect("server write");
        let mut from_server = [0_u8; 32];
        within(client.read_exact(&mut from_server))
            .await
            .expect("client accepts its advertised frame size");
    }

    /// Verifies that an unread channel cannot block traffic on another channel.
    #[tokio::test]
    async fn slow_unread_channel_does_not_block_another_channel() {
        let mut config = test_config();
        config.initial_byte_window = 32;
        config.initial_frame_window = 2;
        let mut pair = pair(config);
        let (mut slow_sender, _slow_receiver) =
            open_channel(&pair.client, &mut pair.server_incoming, "slow").await;
        let (mut fast_sender, mut fast_receiver) =
            open_channel(&pair.client, &mut pair.server_incoming, "fast").await;
        let slow = tokio::spawn(async move { slow_sender.write_all(&[1_u8; 128]).await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!slow.is_finished());

        fast_sender
            .write_all(b"still-live")
            .await
            .expect("fast write");
        let mut message = [0_u8; 10];
        within(fast_receiver.read_exact(&mut message))
            .await
            .expect("fast read");
        assert_eq!(&message, b"still-live");
        slow.abort();
    }

    /// Verifies round-robin fairness between channels with ready data.
    #[tokio::test]
    async fn scheduler_is_fair_between_ready_channels() {
        let mut config = test_config();
        config.initial_byte_window = 1024;
        config.initial_frame_window = 1024;
        config.max_frame_size = 1;
        let mut pair = pair(config);
        let (mut busy_sender, _busy_receiver) =
            open_channel(&pair.client, &mut pair.server_incoming, "busy").await;
        let (mut other_sender, mut other_receiver) =
            open_channel(&pair.client, &mut pair.server_incoming, "other").await;

        busy_sender
            .write_all(&[1_u8; 1024])
            .await
            .expect("busy queue");
        other_sender.write_all(b"x").await.expect("other queue");
        let mut byte = [0_u8; 1];
        within(other_receiver.read_exact(&mut byte))
            .await
            .expect("fairly scheduled channel");
        assert_eq!(byte, *b"x");
    }

    /// Verifies graceful half-closes and asynchronous close-on-drop behavior.
    #[tokio::test]
    async fn shutdown_is_a_half_close_and_drop_flushes_asynchronously() {
        let mut pair = pair(test_config());
        let (mut client, mut server) =
            open_channel(&pair.client, &mut pair.server_incoming, "half-close").await;
        client.write_all(b"request").await.expect("request write");
        client.shutdown().await.expect("client shutdown");
        let mut request = Vec::new();
        within(server.read_to_end(&mut request))
            .await
            .expect("request to EOF");
        assert_eq!(request, b"request");
        server.write_all(b"response").await.expect("response write");
        server.shutdown().await.expect("server shutdown");
        let mut response = Vec::new();
        within(client.read_to_end(&mut response))
            .await
            .expect("response to EOF");
        assert_eq!(response, b"response");

        let close = tokio::spawn(async move { client.close().await });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(
            !close.is_finished(),
            "full close completed before the peer released its channel"
        );
        drop(server);
        within(close)
            .await
            .expect("close task")
            .expect("full channel close");

        let (mut dropped, mut peer) =
            open_channel(&pair.client, &mut pair.server_incoming, "drop").await;
        dropped
            .write_all(b"queued request")
            .await
            .expect("queue request before drop");
        drop(dropped);
        let mut request = Vec::new();
        within(peer.read_to_end(&mut request))
            .await
            .expect("peer reads dropped channel to EOF");
        assert_eq!(request, b"queued request");
        peer.write_all(&[7_u8; 256])
            .await
            .expect("peer response is drained after drop");
        peer.shutdown().await.expect("peer response shutdown");
        within(peer.close())
            .await
            .expect("dropped channel closes in the background");
    }

    /// Verifies that a received FIN wakes an idle writer to send CLOSED.
    #[tokio::test]
    async fn receiving_fin_wakes_an_idle_writer_for_closed() {
        let (io, _peer) = tokio::io::duplex(64);
        let (_driver, handle, _incoming) = connect(io, Role::Client, test_config()).expect("setup");
        let state = {
            let mut core = lock(&handle.core.inner);
            let state = Arc::new(ChannelState {
                id: 1,
                inner: Mutex::new(ChannelInner::new(&core.config)),
            });
            {
                let mut channel = lock(&state.inner);
                channel.fin_sent = true;
                channel.local_close_requested = true;
            }
            core.channels.insert(1, Arc::clone(&state));
            state
        };
        let notified = handle.core.notify.notified();

        receive_fin(&handle.core, 1).expect("receive FIN");
        within(notified).await;
        let selected = select_frame(&handle.core, &mut 0, &mut 0, 0)
            .expect("select")
            .expect("CLOSED frame");
        assert!(matches!(selected.frame, Frame::Closed { id: 1 }));
        assert!(lock(&state.inner).closed_in_flight);
    }

    /// Verifies that asynchronously closed channels release capacity on both
    /// peers.
    #[tokio::test]
    async fn dropped_channels_are_removed_from_both_connection_maps() {
        let mut config = test_config();
        config.max_channels = 1;
        let mut pair = pair(config);
        for index in 0..8 {
            let (local, mut peer) =
                open_channel(&pair.client, &mut pair.server_incoming, "reused-capacity").await;
            drop(local);
            let mut request = Vec::new();
            within(peer.read_to_end(&mut request))
                .await
                .expect("dropped channel reaches EOF");
            assert!(request.is_empty(), "iteration {index}");
            peer.shutdown().await.expect("peer shutdown");
            within(peer.close()).await.expect("peer close");
        }
    }

    /// Verifies that cancelling an open resets it and releases channel
    /// capacity.
    #[tokio::test]
    async fn cancelling_open_resets_the_request_and_releases_capacity() {
        let mut config = test_config();
        config.max_channels = 1;
        let mut pair = pair(config);
        let opener = pair.client.clone();
        let open = tokio::spawn(async move { opener.open("cancelled").await });
        let request = within(pair.server_incoming.recv())
            .await
            .expect("cancelled request");

        open.abort();
        open.await.expect_err("open task is cancelled");
        within(async {
            loop {
                let client_released = {
                    let inner = lock(&pair.client.core.inner);
                    inner.channels.is_empty() && inner.pending_outgoing.is_empty()
                };
                let server_released = lock(&pair.server.core.inner).pending_incoming.is_empty();
                if client_released && server_released {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        drop(request);

        let (_client, _server) =
            open_channel(&pair.client, &mut pair.server_incoming, "replacement").await;
    }

    /// Verifies that simultaneous drops with queued data do not terminate the
    /// connection.
    #[tokio::test]
    async fn queued_data_during_drop_does_not_kill_other_channels() {
        let mut config = test_config();
        config.initial_byte_window = 256;
        config.initial_frame_window = 16;
        let mut pair = pair(config);
        let (mut sender, receiver) =
            open_channel(&pair.client, &mut pair.server_incoming, "aborted").await;
        sender
            .write_all(&[7_u8; 256])
            .await
            .expect("queue aborted data");
        drop(receiver);
        drop(sender);

        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(!pair.client_driver.is_finished());
        assert!(!pair.server_driver.is_finished());
        let (mut recovered, mut peer) =
            open_channel(&pair.client, &mut pair.server_incoming, "recovered").await;
        recovered.write_all(b"ok").await.expect("recovery write");
        let mut message = [0_u8; 2];
        within(peer.read_exact(&mut message))
            .await
            .expect("recovery read");
        assert_eq!(&message, b"ok");
    }

    /// Verifies CLOSED handling when it arrives before FIN write completion.
    #[test]
    fn closed_may_arrive_before_fin_write_completion() {
        let (io, _peer) = tokio::io::duplex(64);
        let (_driver, handle, _incoming) = connect(io, Role::Client, test_config()).expect("setup");
        let state = {
            let mut core = lock(&handle.core.inner);
            let state = Arc::new(ChannelState {
                id: 1,
                inner: Mutex::new(ChannelInner::new(&core.config)),
            });
            {
                let mut channel = lock(&state.inner);
                channel.local_fin_requested = true;
                channel.local_close_requested = true;
            }
            core.channels.insert(1, Arc::clone(&state));
            state
        };

        let fin = select_frame(&handle.core, &mut 0, &mut 0, 0)
            .expect("select")
            .expect("FIN frame");
        assert!(matches!(fin.frame, Frame::Fin { id: 1 }));
        {
            let channel = lock(&state.inner);
            assert!(channel.fin_sent);
            assert!(channel.fin_in_flight);
        }

        // The peer can consume the FIN bytes, finish its own close handshake,
        // and return CLOSED before the local writer future runs completion.
        receive_fin(&handle.core, 1).expect("peer FIN");
        receive_closed(&handle.core, 1).expect("early peer CLOSED");
        assert!(lock(&handle.core.inner).channels.contains_key(&1));
        assert!(
            select_frame(&handle.core, &mut 0, &mut 0, 0)
                .expect("select while FIN is in flight")
                .is_none(),
            "local CLOSED must wait for FIN write completion"
        );

        complete_frame(&handle.core, fin.completion);
        let closed = select_frame(&handle.core, &mut 0, &mut 0, 0)
            .expect("select")
            .expect("CLOSED frame");
        assert!(matches!(closed.frame, Frame::Closed { id: 1 }));
        complete_frame(&handle.core, closed.completion);
        assert!(!lock(&handle.core.inner).channels.contains_key(&1));
    }

    /// Verifies CLOSED ordering behind an in-flight WINDOW before map removal.
    #[tokio::test]
    async fn closed_ack_orders_after_in_flight_window_before_map_removal() {
        let (io, _peer) = tokio::io::duplex(64);
        let (_driver, handle, _incoming) = connect(io, Role::Client, test_config()).expect("setup");
        let state = {
            let mut core = lock(&handle.core.inner);
            let state = Arc::new(ChannelState {
                id: 1,
                inner: Mutex::new(ChannelInner::new(&core.config)),
            });
            {
                let mut channel = lock(&state.inner);
                channel.fin_sent = true;
                channel.window_in_flight = true;
                channel.local_close_requested = true;
            }
            core.channels.insert(1, Arc::clone(&state));
            state
        };

        receive_fin(&handle.core, 1).expect("receive second FIN");
        assert!(lock(&handle.core.inner).channels.contains_key(&1));
        complete_frame(
            &handle.core,
            Completion::Window {
                state: Arc::clone(&state),
                bytes: 0,
                frames: 0,
            },
        );
        let selected = select_frame(&handle.core, &mut 0, &mut 0, 0)
            .expect("select")
            .expect("CLOSED frame");
        assert!(matches!(selected.frame, Frame::Closed { id: 1 }));
        complete_frame(&handle.core, selected.completion);
        assert!(lock(&handle.core.inner).channels.contains_key(&1));
        receive_closed(&handle.core, 1).expect("peer CLOSED acknowledgement");
        assert!(!lock(&handle.core.inner).channels.contains_key(&1));
    }

    /// Verifies that simultaneous opens use disjoint identifier parity.
    #[tokio::test]
    async fn simultaneous_opens_use_disjoint_identifier_parity() {
        let mut pair = pair(test_config());
        let client_open = pair.client.open("from-client");
        let server_open = pair.server.open("from-server");
        let accepts = async {
            let on_server = pair.server_incoming.recv().await.expect("server request");
            let on_client = pair.client_incoming.recv().await.expect("client request");
            (
                on_server.accept().expect("server accept"),
                on_client.accept().expect("client accept"),
            )
        };
        let (client_result, server_result, (server_channel, client_channel)) = within(async {
            let (client_result, server_result, accepted) =
                tokio::join!(client_open, server_open, accepts);
            (client_result, server_result, accepted)
        })
        .await;
        let client_opened = client_result.expect("client open");
        let server_opened = server_result.expect("server open");
        assert_eq!(client_opened.id(), server_channel.id());
        assert_eq!(server_opened.id(), client_channel.id());
        assert_eq!(client_opened.id() % 2, 1);
        assert!(server_opened.id().is_multiple_of(2));
    }

    /// Verifies that surplus probes are ignored before coalesced frames.
    #[tokio::test]
    async fn surplus_probes_before_coalesced_frames_are_ignored() {
        let (io, mut peer) = tokio::io::duplex(4096);
        let (driver, _handle, mut incoming) =
            connect(io, Role::Server, test_config()).expect("setup");
        let driver = tokio::spawn(driver.run());

        peer.write_all(&[KIND_PROBE; 3])
            .await
            .expect("surplus peer probes");
        let mut driver_probe = [u8::MAX; 1];
        peer.read_exact(&mut driver_probe)
            .await
            .expect("driver probe");
        assert_eq!(driver_probe, [KIND_PROBE]);

        let fragmented = wire_frame(KIND_OPEN, 1, &open_payload(64, 4, "fragmented"));
        for byte in fragmented {
            peer.write_all(&[byte]).await.expect("fragmented frame");
        }
        let mut bytes = wire_frame(KIND_OPEN, 3, &open_payload(64, 4, "first"));
        bytes.extend_from_slice(&wire_frame(KIND_OPEN, 5, &open_payload(64, 4, "second")));
        peer.write_all(&bytes).await.expect("coalesced frames");
        assert_eq!(
            within(incoming.recv())
                .await
                .expect("fragmented")
                .endpoint(),
            "fragmented"
        );
        assert_eq!(
            within(incoming.recv()).await.expect("first").endpoint(),
            "first"
        );
        assert_eq!(
            within(incoming.recv()).await.expect("second").endpoint(),
            "second"
        );
        driver.abort();
    }

    /// Verifies malformed frames fail before oversized payload allocation.
    #[tokio::test]
    async fn malformed_and_oversized_frames_fail_before_payload_allocation() {
        for header in [
            malformed_header(99, 1, 0),
            malformed_header(KIND_DATA, 1, test_config().max_frame_size + 1),
            malformed_header(KIND_FIN, 1, 1),
        ] {
            let (driver, mut peer) = manual_connection();
            synchronize_manual(&mut peer).await;
            peer.write_all(&header).await.expect("malformed header");
            let error = within(driver)
                .await
                .expect("driver task")
                .expect_err("protocol error");
            assert!(matches!(error.error(), Error::Protocol(_)));
        }
    }

    /// Verifies that a non-UTF-8 endpoint terminates the connection.
    #[tokio::test]
    async fn invalid_utf8_endpoint_is_a_connection_error() {
        let (driver, mut peer) = manual_connection();
        synchronize_manual(&mut peer).await;
        let mut payload = open_payload(64, 4, "");
        payload.push(0xff);
        peer.write_all(&wire_frame(KIND_OPEN, 1, &payload))
            .await
            .expect("invalid OPEN endpoint");

        let error = within(driver)
            .await
            .expect("driver task")
            .expect_err("protocol error");
        assert!(matches!(error.error(), Error::Protocol(_)));
    }

    /// Verifies that pre-establishment frame data fails and wakes handles.
    #[tokio::test]
    async fn non_probe_before_establishment_is_rejected_and_wakes_handles() {
        let (io, mut peer) = tokio::io::duplex(64);
        let (driver, handle, mut incoming) =
            connect(io, Role::Server, test_config()).expect("setup");
        let driver = tokio::spawn(driver.run());
        let pending = tokio::spawn(async move { handle.open("pending").await });
        peer.write_all(&[KIND_OPEN]).await.expect("non-probe byte");
        let error = within(driver)
            .await
            .expect("driver task")
            .expect_err("invalid synchronization");
        assert!(matches!(error.error(), Error::Protocol(_)));
        assert!(within(pending).await.expect("open task").is_err());
        assert!(within(incoming.recv()).await.is_none());
    }

    /// Verifies that exceeding byte or frame credit terminates the connection.
    #[tokio::test]
    async fn excess_byte_or_frame_credit_is_a_connection_error() {
        for (window_bytes, window_frames, first, excess) in [
            (8, 4, vec![1_u8; 8], vec![2_u8; 1]),
            (64, 1, vec![1_u8; 8], vec![2_u8; 1]),
        ] {
            let mut config = test_config();
            config.initial_byte_window = window_bytes;
            config.initial_frame_window = window_frames;
            config.max_frame_size = 8;
            let (io, mut peer) = tokio::io::duplex(4096);
            let (driver, _handle, mut incoming) = connect(io, Role::Server, config).expect("setup");
            let driver = tokio::spawn(driver.run());
            synchronize_manual(&mut peer).await;
            peer.write_all(&wire_frame(KIND_OPEN, 1, &open_payload(64, 4, "manual")))
                .await
                .expect("open");
            let _unread = within(incoming.recv())
                .await
                .expect("request")
                .accept()
                .expect("accept");
            read_one_wire_frame(&mut peer).await;
            peer.write_all(&wire_frame(KIND_DATA, 1, &first))
                .await
                .expect("within credit");
            peer.write_all(&wire_frame(KIND_DATA, 1, &excess))
                .await
                .expect("excess credit");
            let error = within(driver)
                .await
                .expect("driver task")
                .expect_err("protocol error");
            assert!(matches!(error.error(), Error::Protocol(_)));
        }
    }

    /// Verifies that DATA received before acceptance terminates the connection.
    #[tokio::test]
    async fn data_before_acceptance_is_a_connection_error() {
        let (io, mut peer) = tokio::io::duplex(4096);
        let (driver, _handle, _incoming) =
            connect(io, Role::Server, test_config()).expect("manual setup");
        let driver = tokio::spawn(driver.run());
        synchronize_manual(&mut peer).await;
        peer.write_all(&wire_frame(KIND_OPEN, 1, &open_payload(64, 4, "manual")))
            .await
            .expect("open");
        // The server has not accepted, so DATA is invalid even if the raw
        // advertised receive window would otherwise have room.
        peer.write_all(&wire_frame(KIND_DATA, 1, b"x"))
            .await
            .expect("premature data");
        let error = within(driver)
            .await
            .expect("driver task")
            .expect_err("protocol error");
        assert!(matches!(error.error(), Error::Protocol(_)));
    }

    /// Verifies that transport EOF wakes pending opens and incoming receivers.
    #[tokio::test]
    async fn transport_eof_wakes_pending_open_and_incoming_receiver() {
        let (io, mut peer) = tokio::io::duplex(4096);
        let (driver, handle, mut incoming) =
            connect(io, Role::Server, test_config()).expect("setup");
        let driver = tokio::spawn(driver.run());
        synchronize_manual(&mut peer).await;
        let open = tokio::spawn(async move { handle.open("pending").await });
        let mut header = [0_u8; HEADER_LEN];
        peer.read_exact(&mut header).await.expect("OPEN header");
        let length = usize::try_from(read_u32(&header[8..12])).expect("wire length");
        let mut payload = vec![0_u8; length];
        peer.read_exact(&mut payload).await.expect("OPEN payload");
        drop(peer);

        assert!(within(open).await.expect("open task").is_err());
        assert!(within(incoming.recv()).await.is_none());
        assert!(within(driver).await.expect("driver task").is_err());
    }

    fn manual_connection() -> (JoinHandle<Result<()>>, DuplexStream) {
        let (io, peer) = tokio::io::duplex(4096);
        let (driver, _handle, _incoming) =
            connect(io, Role::Server, test_config()).expect("manual setup");
        (tokio::spawn(driver.run()), peer)
    }

    async fn synchronize_manual(peer: &mut DuplexStream) {
        peer.write_all(&[KIND_PROBE]).await.expect("peer probe");
        let mut probes = [u8::MAX; 2];
        peer.read_exact(&mut probes).await.expect("driver probes");
        assert_eq!(probes, [KIND_PROBE; 2]);
    }

    async fn read_one_wire_frame(peer: &mut DuplexStream) {
        let mut header = [0_u8; HEADER_LEN];
        peer.read_exact(&mut header).await.expect("frame header");
        let length = usize::try_from(read_u32(&header[8..12])).expect("frame length");
        let mut payload = vec![0_u8; length];
        peer.read_exact(&mut payload).await.expect("frame payload");
    }

    fn open_payload(bytes: u32, frames: u32, endpoint: &str) -> Vec<u8> {
        let mut payload = Vec::with_capacity(12 + endpoint.len());
        payload.extend_from_slice(&bytes.to_be_bytes());
        payload.extend_from_slice(&frames.to_be_bytes());
        payload.extend_from_slice(&bytes.min(8).to_be_bytes());
        payload.extend_from_slice(endpoint.as_bytes());
        payload
    }

    fn wire_frame(kind: u8, id: u32, payload: &[u8]) -> Vec<u8> {
        let mut bytes = malformed_header(
            kind,
            id,
            u32::try_from(payload.len()).expect("test payload length"),
        )
        .to_vec();
        bytes.extend_from_slice(payload);
        bytes
    }

    fn malformed_header(kind: u8, id: u32, length: u32) -> [u8; HEADER_LEN] {
        let mut header = [0_u8; HEADER_LEN];
        header[0] = kind;
        header[4..8].copy_from_slice(&id.to_be_bytes());
        header[8..12].copy_from_slice(&length.to_be_bytes());
        header
    }
}
