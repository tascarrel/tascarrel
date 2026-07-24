//! Multiplex protocol synchronization, frames, validation, and wire encoding.
//!
//! [`Frame`] is the validated in-memory representation exchanged with the
//! connection driver. This module owns all wire-format constants and converts
//! between frames and an ordered, reliable byte stream. A standalone zero-byte
//! probe synchronizes each receive direction before framed traffic begins.

use std::io;
use std::time::Duration;

use reportify::ErrorExt as _;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

use crate::Config;
use crate::Error;
use crate::Result;
use crate::io_report;

pub(crate) const HEADER_LEN: usize = 12;
pub(crate) const CHANNEL_PARAMETERS_LEN: usize = 12;

const WINDOW_PAYLOAD_LEN: usize = 8;

/// Standalone synchronization byte sent before framed traffic.
pub(crate) const KIND_PROBE: u8 = 0;
pub(crate) const KIND_OPEN: u8 = 1;
pub(crate) const KIND_ACCEPT: u8 = 2;
pub(crate) const KIND_REJECT: u8 = 3;
pub(crate) const KIND_DATA: u8 = 4;
pub(crate) const KIND_WINDOW: u8 = 5;
pub(crate) const KIND_FIN: u8 = 6;
pub(crate) const KIND_RESET: u8 = 7;
pub(crate) const KIND_CLOSED: u8 = 8;

/// One validated multiplex wire frame.
#[derive(Debug)]
pub(crate) enum Frame {
    Open {
        id: u32,
        byte_window: u32,
        frame_window: u32,
        max_frame_size: u32,
        endpoint: String,
    },
    Accept {
        id: u32,
        byte_window: u32,
        frame_window: u32,
        max_frame_size: u32,
    },
    Reject {
        id: u32,
        reason: Vec<u8>,
    },
    Data {
        id: u32,
        bytes: Vec<u8>,
    },
    Window {
        id: u32,
        bytes: u32,
        frames: u32,
    },
    Fin {
        id: u32,
    },
    Reset {
        id: u32,
        reason: Vec<u8>,
    },
    Closed {
        id: u32,
    },
}

impl Frame {
    /// Returns the frame's channel identifier.
    const fn id(&self) -> u32 {
        match self {
            Self::Open { id, .. }
            | Self::Accept { id, .. }
            | Self::Reject { id, .. }
            | Self::Data { id, .. }
            | Self::Window { id, .. }
            | Self::Fin { id }
            | Self::Reset { id, .. }
            | Self::Closed { id } => *id,
        }
    }
}

/// Repeats probes until one arrives, then sends exactly one reciprocal probe.
#[tracing::instrument(
    name = "tascarrel_mux.protocol.synchronize",
    level = "trace",
    skip(reader, writer),
    fields(?probe_interval),
    err(level = "debug")
)]
pub(crate) async fn probe_handshake<R, W>(
    reader: &mut R,
    writer: &mut W,
    probe_interval: Duration,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut peer = [0_u8; 1];
    loop {
        writer.write_all(&[KIND_PROBE]).await.map_err(io_report)?;
        writer.flush().await.map_err(io_report)?;
        let deadline = tokio::time::sleep(probe_interval);
        tokio::pin!(deadline);
        tokio::select! {
            result = reader.read(&mut peer) => {
                let count = result.map_err(io_report)?;
                if count == 0 {
                    return Err(io_report(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "multiplex transport closed before probe synchronization",
                    )));
                }
                if peer[0] != KIND_PROBE {
                    return Err(Error::Protocol(
                        "non-probe byte before connection establishment".into(),
                    )
                    .report());
                }
                writer.write_all(&[KIND_PROBE]).await.map_err(io_report)?;
                writer.flush().await.map_err(io_report)?;
                return Ok(());
            }
            () = &mut deadline => {}
        }
    }
}

/// Reads and validates one frame without allocating beyond configured limits.
pub(crate) async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    config: &Config,
) -> Result<Frame> {
    let header = read_frame_header(reader).await?;
    let kind = header[0];
    if header[1..4] != [0, 0, 0] {
        return Err(Error::Protocol("non-zero reserved frame bits".into()).report());
    }
    let id = u32::from_be_bytes(header[4..8].try_into().expect("fixed header slice"));
    let length = u32::from_be_bytes(header[8..12].try_into().expect("fixed header slice"));
    if id == 0 {
        return Err(Error::Protocol("channel identifier zero".into()).report());
    }
    let length_usize = usize::try_from(length)
        .map_err(|error| error.escalate(Error::Protocol("invalid frame length".into())))?;
    let valid_length = match kind {
        KIND_OPEN => (CHANNEL_PARAMETERS_LEN..=CHANNEL_PARAMETERS_LEN + config.max_endpoint_size)
            .contains(&length_usize),
        KIND_ACCEPT => length_usize == CHANNEL_PARAMETERS_LEN,
        KIND_WINDOW => length_usize == WINDOW_PAYLOAD_LEN,
        KIND_REJECT | KIND_RESET => length_usize <= config.max_reason_size,
        KIND_DATA => length > 0 && length <= config.max_frame_size,
        KIND_FIN | KIND_CLOSED => length == 0,
        _ => return Err(Error::Protocol(format!("unknown frame kind {kind}")).report()),
    };
    if !valid_length {
        return Err(
            Error::Protocol(format!("invalid length {length} for frame kind {kind}")).report(),
        );
    }
    let mut payload = vec![0_u8; length_usize];
    reader.read_exact(&mut payload).await.map_err(io_report)?;
    match kind {
        KIND_OPEN => {
            let byte_window = read_u32(&payload[0..4]);
            let frame_window = read_u32(&payload[4..8]);
            let max_frame_size = read_u32(&payload[8..12]);
            validate_window(byte_window, frame_window, max_frame_size)?;
            let endpoint = std::str::from_utf8(&payload[CHANNEL_PARAMETERS_LEN..])
                .map_err(|error| {
                    error.escalate(Error::Protocol("channel endpoint is not UTF-8".into()))
                })?
                .to_owned();
            Ok(Frame::Open {
                id,
                byte_window,
                frame_window,
                max_frame_size,
                endpoint,
            })
        }
        KIND_ACCEPT => {
            let byte_window = read_u32(&payload[0..4]);
            let frame_window = read_u32(&payload[4..8]);
            let max_frame_size = read_u32(&payload[8..12]);
            validate_window(byte_window, frame_window, max_frame_size)?;
            Ok(Frame::Accept {
                id,
                byte_window,
                frame_window,
                max_frame_size,
            })
        }
        KIND_REJECT => Ok(Frame::Reject {
            id,
            reason: payload,
        }),
        KIND_DATA => Ok(Frame::Data { id, bytes: payload }),
        KIND_WINDOW => {
            let bytes = read_u32(&payload[0..4]);
            let frames = read_u32(&payload[4..8]);
            if bytes == 0 && frames == 0 {
                return Err(Error::Protocol("empty window update".into()).report());
            }
            Ok(Frame::Window { id, bytes, frames })
        }
        KIND_FIN => Ok(Frame::Fin { id }),
        KIND_RESET => Ok(Frame::Reset {
            id,
            reason: payload,
        }),
        KIND_CLOSED => Ok(Frame::Closed { id }),
        _ => unreachable!("frame kind was validated"),
    }
}

/// Encodes, writes, and flushes one complete frame.
pub(crate) async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &Frame,
) -> Result<()> {
    let (kind, payload) = match frame {
        Frame::Open {
            byte_window,
            frame_window,
            max_frame_size,
            endpoint,
            ..
        } => {
            let mut payload = Vec::with_capacity(CHANNEL_PARAMETERS_LEN + endpoint.len());
            payload.extend_from_slice(&byte_window.to_be_bytes());
            payload.extend_from_slice(&frame_window.to_be_bytes());
            payload.extend_from_slice(&max_frame_size.to_be_bytes());
            payload.extend_from_slice(endpoint.as_bytes());
            (KIND_OPEN, payload)
        }
        Frame::Accept {
            byte_window,
            frame_window,
            max_frame_size,
            ..
        } => {
            let mut payload = Vec::with_capacity(CHANNEL_PARAMETERS_LEN);
            payload.extend_from_slice(&byte_window.to_be_bytes());
            payload.extend_from_slice(&frame_window.to_be_bytes());
            payload.extend_from_slice(&max_frame_size.to_be_bytes());
            (KIND_ACCEPT, payload)
        }
        Frame::Reject { reason, .. } => (KIND_REJECT, reason.clone()),
        Frame::Data { bytes, .. } => (KIND_DATA, bytes.clone()),
        Frame::Window { bytes, frames, .. } => {
            let mut payload = Vec::with_capacity(WINDOW_PAYLOAD_LEN);
            payload.extend_from_slice(&bytes.to_be_bytes());
            payload.extend_from_slice(&frames.to_be_bytes());
            (KIND_WINDOW, payload)
        }
        Frame::Fin { .. } => (KIND_FIN, Vec::new()),
        Frame::Reset { reason, .. } => (KIND_RESET, reason.clone()),
        Frame::Closed { .. } => (KIND_CLOSED, Vec::new()),
    };
    let length =
        u32::try_from(payload.len()).map_err(|error| error.escalate(Error::InvalidInput))?;
    let mut header = [0_u8; HEADER_LEN];
    header[0] = kind;
    header[4..8].copy_from_slice(&frame.id().to_be_bytes());
    header[8..12].copy_from_slice(&length.to_be_bytes());
    writer.write_all(&header).await.map_err(io_report)?;
    writer.write_all(&payload).await.map_err(io_report)?;
    writer.flush().await.map_err(io_report)
}

/// Reads a header while discarding surplus probes at frame boundaries.
async fn read_frame_header<R: AsyncRead + Unpin>(reader: &mut R) -> Result<[u8; HEADER_LEN]> {
    loop {
        let mut header = [0_u8; HEADER_LEN];
        reader
            .read_exact(&mut header[..1])
            .await
            .map_err(io_report)?;
        if header[0] == KIND_PROBE {
            continue;
        }
        reader
            .read_exact(&mut header[1..])
            .await
            .map_err(io_report)?;
        return Ok(header);
    }
}

/// Validates peer-advertised channel window bounds.
fn validate_window(bytes: u32, frames: u32, max_frame_size: u32) -> Result<()> {
    if bytes == 0 || frames == 0 || max_frame_size == 0 || max_frame_size > bytes {
        return Err(Error::Protocol("invalid advertised window".into()).report());
    }
    Ok(())
}

/// Decodes one network-order integer from a validated four-byte slice.
pub(crate) const fn read_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}
