//! Transport abstraction and byte-stream transport for control-plane messages.

use std::future::Future;

use bytes::Bytes;
use futures_util::SinkExt as _;
use futures_util::StreamExt as _;
use reportify::ErrorExt as _;
use tascarrel_api::types::protocol::Message;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio_util::codec::Framed;
use tokio_util::codec::LengthDelimitedCodec;

use super::Error;
use super::Result;
use crate::DEFAULT_MAX_FRAME_LEN;

/// A framed transport carrying complete control plane messages.
pub trait Transport: Send {
    /// Receives the next message or reports a clean end of stream.
    fn receive(&mut self) -> impl Future<Output = Result<Option<Message>>> + Send;

    /// Sends one complete message.
    fn send(&mut self, message: Message) -> impl Future<Output = Result<()>> + Send;
}

/// Carries control messages over a length-delimited JSON byte stream.
///
/// A Tascarrel mux channel implements the required byte-stream traits and can
/// therefore be passed directly to this transport.
#[derive(Debug)]
pub struct StreamTransport<T> {
    framed: Framed<T, LengthDelimitedCodec>,
}

impl<T> StreamTransport<T> {
    /// Creates a transport with [`DEFAULT_MAX_FRAME_LEN`].
    #[must_use]
    pub fn new(io: T) -> Self {
        let codec = LengthDelimitedCodec::builder()
            .max_frame_length(DEFAULT_MAX_FRAME_LEN)
            .new_codec();
        Self {
            framed: Framed::new(io, codec),
        }
    }

    /// Creates a transport with a custom maximum JSON payload length.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidMaxFrameLen`] for an invalid limit.
    pub fn with_max_frame_len(io: T, max_frame_len: usize) -> Result<Self> {
        if max_frame_len == 0 || u32::try_from(max_frame_len).is_err() {
            return Err(Error::InvalidMaxFrameLen.report());
        }
        let codec = LengthDelimitedCodec::builder()
            .max_frame_length(max_frame_len)
            .new_codec();
        Ok(Self {
            framed: Framed::new(io, codec),
        })
    }

    /// Returns the configured maximum JSON payload length.
    #[must_use]
    pub fn max_frame_len(&self) -> usize {
        self.framed.codec().max_frame_length()
    }

    /// Returns the underlying byte stream.
    #[must_use]
    pub fn get_ref(&self) -> &T {
        self.framed.get_ref()
    }

    /// Returns the underlying byte stream mutably.
    pub fn get_mut(&mut self) -> &mut T {
        self.framed.get_mut()
    }

    /// Consumes the transport and returns the underlying byte stream.
    #[must_use]
    pub fn into_inner(self) -> T {
        self.framed.into_inner()
    }
}

impl<T> Transport for StreamTransport<T>
where
    T: AsyncRead + AsyncWrite + Unpin + Send,
{
    async fn receive(&mut self) -> Result<Option<Message>> {
        let Some(frame) = self.framed.next().await else {
            return Ok(None);
        };
        let frame = frame.map_err(|error| error.escalate(Error::Transport))?;
        serde_json::from_slice(&frame)
            .map(Some)
            .map_err(|error| error.escalate(Error::InvalidMessage))
    }

    async fn send(&mut self, message: Message) -> Result<()> {
        let frame =
            serde_json::to_vec(&message).map_err(|error| error.escalate(Error::InvalidMessage))?;
        let max_frame_len = self.framed.codec().max_frame_length();
        if frame.len() > max_frame_len {
            return Err(Error::FrameTooLarge {
                len: frame.len(),
                max: max_frame_len,
            }
            .report());
        }
        self.framed
            .send(Bytes::from(frame))
            .await
            .map_err(|error| error.escalate(Error::Transport))
    }
}

#[cfg(test)]
mod tests {
    use tascarrel_api::types::protocol as wire;
    use tokio::io::duplex;

    use super::*;

    /// Verifies that the byte-stream transport preserves complete messages.
    #[tokio::test]
    async fn stream_transport_round_trips_control_messages() {
        let (left, right) = duplex(4096);
        let mut sender = StreamTransport::new(left);
        let mut receiver = StreamTransport::new(right);
        let expected = ping();

        sender.send(expected.clone()).await.expect("send ping");
        let actual = receiver
            .receive()
            .await
            .expect("receive ping")
            .expect("stream remains open");

        assert_eq!(actual, expected);
    }

    fn ping() -> Message {
        Message::Control(wire::ControlMessage::Ping(wire::PingMessage { data: None }))
    }
}
