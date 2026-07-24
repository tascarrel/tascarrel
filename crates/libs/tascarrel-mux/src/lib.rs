//! A small, symmetric stream multiplexer for Tascarrel transports.
//!
//! The wire format is deliberately transport-agnostic. After synchronization,
//! it requires an ordered, reliable byte stream. Each logical channel has
//! independent byte and frame windows. Credit is returned only after the
//! application consumes received data, so a slow channel cannot consume
//! unbounded memory or stop unrelated channels.
//!
//! A connection separates transport progress ([`Driver`]) from application
//! control ([`MuxHandle`] and [`Incoming`]). Logical [`Channel`]s implement
//! Tokio's byte-stream traits; graceful shutdown closes one write half, while
//! dropping an unfinished channel resets it.
//!
//! # Transport Synchronization
//!
//! Before either peer writes frames, both repeatedly write a standalone zero
//! probe byte. After receiving a probe, a peer writes and flushes exactly one
//! reciprocal probe and enters framed mode.
//!
//! This handshake accommodates virtio-serial transports, whose endpoints may
//! attach at different times and discard bytes written before peer attachment.
//!
//! A transport may discard probes before establishing a path. Once it delivers
//! a probe from side A to side B, it must deliver the next probe side B sends
//! to side A. The same reciprocal guarantee then applies from side A to side B.
//! Delivered probes and all following bytes in their direction must remain
//! ordered and lossless through EOF. Surplus probes are ignored only at frame
//! boundaries.
//!
//! Closing an established transport must also isolate connection generations:
//! non-probe bytes from the closed transport must never appear on a later
//! transport instance. Buffered probes are harmless because every new peer
//! performs the same synchronization.
//!
//! ```
//! use tascarrel_mux::Config;
//! use tascarrel_mux::Role;
//! use tascarrel_mux::connect;
//!
//! #[tokio::main(flavor = "current_thread")]
//! async fn main() {
//!     let (client_io, server_io) = tokio::io::duplex(4096);
//!     let (client_driver, client, _client_incoming) =
//!         connect(client_io, Role::Client, Config::default())
//!             .expect("client configuration is valid");
//!     let (server_driver, _server, mut server_incoming) =
//!         connect(server_io, Role::Server, Config::default())
//!             .expect("server configuration is valid");
//!     let client_driver = tokio::spawn(client_driver.run());
//!     let server_driver = tokio::spawn(server_driver.run());
//!
//!     let (client_channel, server_channel) = tokio::join!(
//!         client.open("control"),
//!         async {
//!             server_incoming
//!                 .recv()
//!                 .await
//!                 .expect("client opens a channel")
//!                 .accept()
//!         },
//!     );
//!     drop(client_channel.expect("server accepts the channel"));
//!     drop(server_channel.expect("server can accept the channel"));
//!
//!     client_driver.abort();
//!     server_driver.abort();
//!     assert!(client_driver.await.expect_err("client driver was aborted").is_cancelled());
//!     assert!(server_driver.await.expect_err("server driver was aborted").is_cancelled());
//! }
//! ```

#![deny(unsafe_code)]

use std::io;
use std::time::Duration;

use reportify::ErrorExt as _;
use reportify::Report;
use thiserror::Error;

mod connection;
mod driver;
mod protocol;

pub use connection::Channel;
pub use connection::Incoming;
pub use connection::IncomingRequest;
pub use connection::MuxHandle;
pub use connection::connect;
pub use driver::Driver;
use protocol::CHANNEL_PARAMETERS_LEN;

/// Which side of a connection allocates odd channel identifiers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    /// Allocates odd channel identifiers.
    Client,
    /// Allocates even channel identifiers.
    Server,
}

impl Role {
    /// Returns this side's first channel identifier.
    const fn first_id(self) -> u32 {
        match self {
            Self::Client => 1,
            Self::Server => 2,
        }
    }

    /// Reports whether this side owns a channel identifier.
    const fn owns_id(self, id: u32) -> bool {
        id != 0
            && match self {
                Self::Client => id % 2 == 1,
                Self::Server => id.is_multiple_of(2),
            }
    }
}

/// Resource, flow-control, and scheduling settings for a connection.
#[derive(Clone, Debug)]
pub struct Config {
    /// Bytes a peer may send on a channel before receiving updates.
    pub initial_byte_window: u32,
    /// Frames a peer may send on a channel before receiving updates.
    pub initial_frame_window: u32,
    /// Largest DATA payload accepted and advertised to the peer. The sender
    /// uses the value negotiated in OPEN/ACCEPT, so peers may configure
    /// different limits safely.
    pub max_frame_size: u32,
    /// Maximum number of active and pending channels.
    pub max_channels: usize,
    /// Maximum OPEN endpoint length in UTF-8 bytes.
    pub max_endpoint_size: usize,
    /// Maximum REJECT or RESET reason length.
    pub max_reason_size: usize,
    /// Maximum number of queued connection-control frames.
    pub control_queue_capacity: usize,
    /// Number of control frames written before ready data gets priority.
    pub control_burst: usize,
    /// Number of recently closed channel identifiers retained for late frames.
    pub closed_channel_capacity: usize,
    /// Delay between synchronization probes while waiting for a peer.
    pub probe_interval: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            initial_byte_window: 256 * 1024,
            initial_frame_window: 64,
            max_frame_size: 16 * 1024,
            max_channels: 64,
            max_endpoint_size: 256,
            max_reason_size: 256,
            control_queue_capacity: 272,
            control_burst: 16,
            closed_channel_capacity: 256,
            probe_interval: Duration::from_millis(100),
        }
    }
}

impl Config {
    /// Validates relationships and representability of configured values.
    fn validate(&self) -> Result<()> {
        let endpoint_length_is_representable = self
            .max_endpoint_size
            .checked_add(CHANNEL_PARAMETERS_LEN)
            .is_some_and(|length| u32::try_from(length).is_ok());
        if self.initial_byte_window == 0
            || self.initial_frame_window == 0
            || self.max_frame_size == 0
            || self.max_frame_size > self.initial_byte_window
            || self.max_channels == 0
            || self.max_channels > tokio::sync::Semaphore::MAX_PERMITS
            || self.max_endpoint_size == 0
            || !endpoint_length_is_representable
            || self.max_reason_size == 0
            || u32::try_from(self.max_reason_size).is_err()
            || self.control_queue_capacity == 0
            || self.control_burst == 0
            || self.closed_channel_capacity == 0
            || self.probe_interval.is_zero()
        {
            return Err(Error::InvalidConfig.report());
        }
        Ok(())
    }
}

/// A typed connection or channel operation error carried by [`Report`].
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum Error {
    #[error("multiplex transport I/O failed")]
    Io,
    #[error("multiplex protocol error: {0}")]
    Protocol(String),
    #[error("multiplex connection is closed")]
    ConnectionClosed,
    #[error("multiplex resource limit reached")]
    ResourceExhausted,
    #[error("channel was rejected: {0}")]
    Rejected(String),
    #[error("channel was reset: {0}")]
    Reset(String),
    #[error("invalid multiplex configuration")]
    InvalidConfig,
    #[error("invalid endpoint or reason")]
    InvalidInput,
}

/// The result type returned by multiplex operations.
pub type Result<T> = std::result::Result<T, Report<Error>>;

/// Creates a fresh report for a stored terminal error.
#[track_caller]
fn terminal_report(error: &Error) -> Report<Error> {
    error.clone().report()
}

/// Classifies and preserves an underlying transport error.
#[track_caller]
fn io_report(error: io::Error) -> Report<Error> {
    let kind = if matches!(
        error.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::NotConnected
    ) {
        Error::ConnectionClosed
    } else {
        Error::Io
    };
    error
        .escalate(kind)
        .message("multiplex transport operation failed")
}
