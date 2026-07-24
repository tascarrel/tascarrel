//! Wire protocols shared by Tascarrel clients and daemons.
//!
//! The [`control_plane`] module implements the control plane over pluggable
//! transports. [`control_plane::connect`] exposes link-level lifecycle
//! primitives, [`control_plane::server::Server`] serves and routes operations,
//! and [`control_plane::StreamTransport`] carries the protocol over
//! asynchronous byte streams.
//!
//! Dedicated multiplex endpoints carry streaming chat attachments, pod
//! workspace file bodies, and validated workspace input snapshots outside the
//! control plane.
//!
//! Dedicated streaming messages use UTF-8 JSON preceded by a fixed 12-byte
//! header. The header consists of [`MAGIC`], a big-endian [`PROTOCOL_VERSION`],
//! two reserved flag bytes, and a big-endian `u32` payload length. Receivers
//! reject the payload length before allocating it.

mod codec;
pub mod control_plane;
mod data_plane;

pub use codec::BlockingFramed;
pub use codec::CodecError;
pub use codec::FrameReader;
pub use codec::FrameWriter;
pub use codec::Framed;
pub use control_plane::MUX_CONTROL_PLANE_ENDPOINT;
pub use data_plane::network;
pub use data_plane::workspace::snapshot as workspace_snapshot;
pub use data_plane::*;

/// Magic bytes at the start of every frame.
pub const MAGIC: [u8; 4] = *b"TSC\0";

/// Current on-wire protocol version.
pub const PROTOCOL_VERSION: u16 = 15;

/// Number of bytes in a frame header.
pub const HEADER_LEN: usize = 12;

/// Default maximum JSON payload accepted or emitted by a codec.
///
/// This accommodates the host configuration API's 4 MiB source-file limit
/// together with its JSON envelope and encoding overhead.
pub const DEFAULT_MAX_FRAME_LEN: usize = 8 * 1024 * 1024;
