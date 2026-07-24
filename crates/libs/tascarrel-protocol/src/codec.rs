use std::io;
use std::io::Read;
use std::io::Write;

use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::ReadHalf;
use tokio::io::WriteHalf;

use crate::DEFAULT_MAX_FRAME_LEN;
use crate::HEADER_LEN;
use crate::MAGIC;
use crate::PROTOCOL_VERSION;

/// A framing or serialization failure.
#[derive(Debug, Error)]
pub enum CodecError {
    #[error("frame I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("invalid frame magic: {0:?}")]
    InvalidMagic([u8; 4]),
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u16),
    #[error("unsupported frame flags 0x{0:04x}")]
    UnsupportedFlags(u16),
    #[error("frame payload is {len} bytes, exceeding the {max} byte limit")]
    FrameTooLarge { len: usize, max: usize },
    #[error("maximum frame length must be in 1..={}", u32::MAX)]
    InvalidMaxFrameLen,
    #[error("could not encode frame JSON: {0}")]
    Encode(#[source] serde_json::Error),
    #[error("could not decode frame JSON: {0}")]
    Decode(#[source] serde_json::Error),
}

/// A bidirectional framed transport.
#[derive(Debug)]
pub struct Framed<T> {
    io: T,
    max_frame_len: usize,
}

/// A framed transport for blocking readers and writers.
///
/// This is the synchronous counterpart to [`Framed`] for helpers which Git
/// invokes outside an async runtime.
#[derive(Debug)]
pub struct BlockingFramed<T> {
    io: T,
    max_frame_len: usize,
}

impl<T> BlockingFramed<T> {
    #[must_use]
    pub fn new(io: T) -> Self {
        Self {
            io,
            max_frame_len: DEFAULT_MAX_FRAME_LEN,
        }
    }

    /// Uses a custom payload limit.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError::InvalidMaxFrameLen`] for zero or a value larger
    /// than the `u32` wire length field.
    pub fn with_max_frame_len(io: T, max_frame_len: usize) -> Result<Self, CodecError> {
        validate_limit(max_frame_len)?;
        Ok(Self { io, max_frame_len })
    }

    #[must_use]
    pub const fn max_frame_len(&self) -> usize {
        self.max_frame_len
    }

    #[must_use]
    pub const fn get_ref(&self) -> &T {
        &self.io
    }

    pub const fn get_mut(&mut self) -> &mut T {
        &mut self.io
    }

    #[must_use]
    pub fn into_inner(self) -> T {
        self.io
    }
}

impl<T: Read> BlockingFramed<T> {
    /// Reads one message, returning `None` only for EOF between frames.
    ///
    /// # Errors
    ///
    /// Returns a codec error for malformed, unsupported, oversized, truncated,
    /// or otherwise unreadable frames.
    pub fn read<M: DeserializeOwned>(&mut self) -> Result<Option<M>, CodecError> {
        let Some(header) = read_blocking_header(&mut self.io)? else {
            return Ok(None);
        };
        let payload_len = validate_header(&header, self.max_frame_len)?;
        let mut payload = vec![0; payload_len];
        self.io.read_exact(&mut payload)?;
        serde_json::from_slice(&payload)
            .map(Some)
            .map_err(CodecError::Decode)
    }
}

impl<T: Write> BlockingFramed<T> {
    /// Writes and flushes one complete message.
    ///
    /// # Errors
    ///
    /// Returns a codec error if serialization fails, the payload exceeds the
    /// configured limit, or the stream cannot be written.
    pub fn write<M: Serialize>(&mut self, message: &M) -> Result<(), CodecError> {
        let (header, payload) = encode_message(self.max_frame_len, message)?;
        self.io.write_all(&header)?;
        self.io.write_all(&payload)?;
        self.io.flush()?;
        Ok(())
    }
}

impl<T> Framed<T> {
    #[must_use]
    pub fn new(io: T) -> Self {
        Self {
            io,
            max_frame_len: DEFAULT_MAX_FRAME_LEN,
        }
    }

    /// Uses a custom payload limit.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError::InvalidMaxFrameLen`] for zero or a value larger
    /// than the `u32` wire length field.
    pub fn with_max_frame_len(io: T, max_frame_len: usize) -> Result<Self, CodecError> {
        validate_limit(max_frame_len)?;
        Ok(Self { io, max_frame_len })
    }

    #[must_use]
    pub const fn max_frame_len(&self) -> usize {
        self.max_frame_len
    }

    #[must_use]
    pub const fn get_ref(&self) -> &T {
        &self.io
    }

    pub const fn get_mut(&mut self) -> &mut T {
        &mut self.io
    }

    #[must_use]
    pub fn into_inner(self) -> T {
        self.io
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> Framed<T> {
    /// Reads one message, returning `None` only for EOF between frames.
    ///
    /// # Errors
    ///
    /// Returns a codec error for malformed, unsupported, oversized, truncated,
    /// or otherwise unreadable frames.
    pub async fn read<M: DeserializeOwned>(&mut self) -> Result<Option<M>, CodecError> {
        read_message(&mut self.io, self.max_frame_len).await
    }

    /// Writes and flushes one complete message.
    ///
    /// # Errors
    ///
    /// Returns a codec error if serialization fails, the payload exceeds the
    /// configured limit, or the stream cannot be written.
    pub async fn write<M: Serialize>(&mut self, message: &M) -> Result<(), CodecError> {
        write_message(&mut self.io, self.max_frame_len, message).await
    }

    /// Splits the transport while preserving its configured length limit.
    #[must_use]
    pub fn split(self) -> (FrameReader<ReadHalf<T>>, FrameWriter<WriteHalf<T>>) {
        let (reader, writer) = tokio::io::split(self.io);
        (
            FrameReader {
                io: reader,
                max_frame_len: self.max_frame_len,
            },
            FrameWriter {
                io: writer,
                max_frame_len: self.max_frame_len,
            },
        )
    }
}

/// The read half of a framed transport.
#[derive(Debug)]
pub struct FrameReader<R> {
    io: R,
    max_frame_len: usize,
}

impl<R> FrameReader<R> {
    #[must_use]
    pub fn new(io: R) -> Self {
        Self {
            io,
            max_frame_len: DEFAULT_MAX_FRAME_LEN,
        }
    }

    /// Uses a custom payload limit.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError::InvalidMaxFrameLen`] for an invalid limit.
    pub fn with_max_frame_len(io: R, max_frame_len: usize) -> Result<Self, CodecError> {
        validate_limit(max_frame_len)?;
        Ok(Self { io, max_frame_len })
    }

    #[must_use]
    pub fn into_inner(self) -> R {
        self.io
    }
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    /// Reads one message, returning `None` only for EOF between frames.
    ///
    /// # Errors
    ///
    /// Returns a codec error for malformed, unsupported, oversized, truncated,
    /// or otherwise unreadable frames.
    pub async fn read<M: DeserializeOwned>(&mut self) -> Result<Option<M>, CodecError> {
        read_message(&mut self.io, self.max_frame_len).await
    }
}

/// The write half of a framed transport.
#[derive(Debug)]
pub struct FrameWriter<W> {
    io: W,
    max_frame_len: usize,
}

impl<W> FrameWriter<W> {
    #[must_use]
    pub fn new(io: W) -> Self {
        Self {
            io,
            max_frame_len: DEFAULT_MAX_FRAME_LEN,
        }
    }

    /// Uses a custom payload limit.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError::InvalidMaxFrameLen`] for an invalid limit.
    pub fn with_max_frame_len(io: W, max_frame_len: usize) -> Result<Self, CodecError> {
        validate_limit(max_frame_len)?;
        Ok(Self { io, max_frame_len })
    }

    #[must_use]
    pub fn into_inner(self) -> W {
        self.io
    }
}

impl<W: AsyncWrite + Unpin> FrameWriter<W> {
    /// Writes and flushes one complete message.
    ///
    /// # Errors
    ///
    /// Returns a codec error if serialization fails, the payload exceeds the
    /// configured limit, or the stream cannot be written.
    pub async fn write<M: Serialize>(&mut self, message: &M) -> Result<(), CodecError> {
        write_message(&mut self.io, self.max_frame_len, message).await
    }
}

fn validate_limit(limit: usize) -> Result<(), CodecError> {
    if limit == 0 || u32::try_from(limit).is_err() {
        Err(CodecError::InvalidMaxFrameLen)
    } else {
        Ok(())
    }
}

async fn read_message<R: AsyncRead + Unpin, M: DeserializeOwned>(
    reader: &mut R,
    max_frame_len: usize,
) -> Result<Option<M>, CodecError> {
    let Some(header) = read_header(reader).await? else {
        return Ok(None);
    };

    let payload_len = validate_header(&header, max_frame_len)?;

    let mut payload = vec![0; payload_len];
    reader.read_exact(&mut payload).await?;
    serde_json::from_slice(&payload)
        .map(Some)
        .map_err(CodecError::Decode)
}

async fn read_header<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<[u8; HEADER_LEN]>, CodecError> {
    let mut header = [0; HEADER_LEN];
    if reader.read(&mut header[..1]).await? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut header[1..]).await?;
    Ok(Some(header))
}

async fn write_message<W: AsyncWrite + Unpin, M: Serialize>(
    writer: &mut W,
    max_frame_len: usize,
    message: &M,
) -> Result<(), CodecError> {
    let (header, payload) = encode_message(max_frame_len, message)?;
    writer.write_all(&header).await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

fn validate_header(header: &[u8; HEADER_LEN], max_frame_len: usize) -> Result<usize, CodecError> {
    let magic = header[..4].try_into().expect("header field has fixed size");
    if magic != MAGIC {
        return Err(CodecError::InvalidMagic(magic));
    }

    let version = u16::from_be_bytes(header[4..6].try_into().expect("fixed-size field"));
    if version != PROTOCOL_VERSION {
        return Err(CodecError::UnsupportedVersion(version));
    }

    let flags = u16::from_be_bytes(header[6..8].try_into().expect("fixed-size field"));
    if flags != 0 {
        return Err(CodecError::UnsupportedFlags(flags));
    }

    let payload_len =
        u32::from_be_bytes(header[8..12].try_into().expect("fixed-size field")) as usize;
    if payload_len > max_frame_len {
        return Err(CodecError::FrameTooLarge {
            len: payload_len,
            max: max_frame_len,
        });
    }
    Ok(payload_len)
}

fn encode_message<M: Serialize>(
    max_frame_len: usize,
    message: &M,
) -> Result<([u8; HEADER_LEN], Vec<u8>), CodecError> {
    let payload = serde_json::to_vec(message).map_err(CodecError::Encode)?;
    if payload.len() > max_frame_len {
        return Err(CodecError::FrameTooLarge {
            len: payload.len(),
            max: max_frame_len,
        });
    }
    let payload_len = u32::try_from(payload.len()).map_err(|_| CodecError::FrameTooLarge {
        len: payload.len(),
        max: max_frame_len,
    })?;

    let mut header = [0; HEADER_LEN];
    header[..4].copy_from_slice(&MAGIC);
    header[4..6].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    header[6..8].copy_from_slice(&0_u16.to_be_bytes());
    header[8..12].copy_from_slice(&payload_len.to_be_bytes());
    Ok((header, payload))
}

fn read_blocking_header(reader: &mut impl Read) -> Result<Option<[u8; HEADER_LEN]>, CodecError> {
    let mut header = [0; HEADER_LEN];
    if reader.read(&mut header[..1])? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut header[1..])?;
    Ok(Some(header))
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::io::ErrorKind;

    use tokio::io::AsyncWriteExt;
    use tokio::io::duplex;

    use super::*;

    #[derive(Debug, PartialEq, serde::Deserialize, serde::Serialize)]
    struct TestFrame {
        id: u64,
        message: String,
    }

    fn test_frame() -> TestFrame {
        TestFrame {
            id: 42,
            message: "ping".to_owned(),
        }
    }

    /// Verifies blocking helpers use the same framing as async daemon links.
    #[test]
    fn blocking_codec_round_trips_a_message() {
        let expected = test_frame();
        let mut sender = BlockingFramed::new(Vec::new());
        sender.write(&expected).unwrap();
        let mut receiver = BlockingFramed::new(Cursor::new(sender.into_inner()));

        let actual: TestFrame = receiver.read().unwrap().unwrap();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn round_trips_over_a_character_stream() {
        let (left, right) = duplex(4096);
        let mut sender = Framed::new(left);
        let mut receiver = Framed::new(right);
        let expected = test_frame();

        sender.write(&expected).await.unwrap();
        let actual: TestFrame = receiver.read().await.unwrap().unwrap();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn clean_eof_is_distinct_from_a_truncated_header() {
        let (left, right) = duplex(64);
        drop(left);
        let mut receiver = Framed::new(right);
        assert!(receiver.read::<TestFrame>().await.unwrap().is_none());

        let (mut left, right) = duplex(64);
        left.write_all(&MAGIC[..2]).await.unwrap();
        drop(left);
        let mut receiver = Framed::new(right);
        let error = receiver.read::<TestFrame>().await.unwrap_err();
        assert!(matches!(error, CodecError::Io(ref io) if io.kind() == ErrorKind::UnexpectedEof));
    }

    #[tokio::test]
    async fn rejects_oversize_before_reading_payload() {
        let (mut left, right) = duplex(64);
        let mut header = [0; HEADER_LEN];
        header[..4].copy_from_slice(&MAGIC);
        header[4..6].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        header[8..12].copy_from_slice(&100_u32.to_be_bytes());
        left.write_all(&header).await.unwrap();

        let mut receiver = Framed::with_max_frame_len(right, 10).unwrap();
        let error = receiver.read::<TestFrame>().await.unwrap_err();
        assert!(matches!(
            error,
            CodecError::FrameTooLarge { len: 100, max: 10 }
        ));
    }

    #[tokio::test]
    async fn rejects_bad_magic_version_and_flags() {
        for (header, assertion) in [
            (
                {
                    let mut header = valid_empty_header();
                    header[0] = b'X';
                    header
                },
                0,
            ),
            (
                {
                    let mut header = valid_empty_header();
                    header[4..6].copy_from_slice(&(PROTOCOL_VERSION + 1).to_be_bytes());
                    header
                },
                1,
            ),
            (
                {
                    let mut header = valid_empty_header();
                    header[6..8].copy_from_slice(&1_u16.to_be_bytes());
                    header
                },
                2,
            ),
        ] {
            let (mut left, right) = duplex(64);
            left.write_all(&header).await.unwrap();
            let mut receiver = Framed::new(right);
            let error = receiver.read::<TestFrame>().await.unwrap_err();
            match assertion {
                0 => assert!(matches!(error, CodecError::InvalidMagic(_))),
                1 => assert!(matches!(error, CodecError::UnsupportedVersion(_))),
                2 => assert!(matches!(error, CodecError::UnsupportedFlags(_))),
                _ => unreachable!(),
            }
        }
    }

    #[tokio::test]
    async fn writer_enforces_its_limit() {
        let (left, _right) = duplex(64);
        let mut writer = Framed::with_max_frame_len(left, 8).unwrap();
        let error = writer.write(&test_frame()).await.unwrap_err();
        assert!(matches!(error, CodecError::FrameTooLarge { max: 8, .. }));
    }

    fn valid_empty_header() -> [u8; HEADER_LEN] {
        let mut header = [0; HEADER_LEN];
        header[..4].copy_from_slice(&MAGIC);
        header[4..6].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        header
    }
}
