//! Packet operations independent of byte-stream I/O.

use async_trait::async_trait;
use bevy::platform::time::Instant;
use std::error::Error;
use std::fmt::{Debug, Formatter};
use std::io;
use std::sync::Arc;

use crate::connection::ReceiveLimits;
use crate::serializers::serializer::Serializer;
use crate::PacketLengthSerializer;

/// Receives decoded packets from a transport.
///
/// Byte transports can use [`FramedReader`](super::protocol::FramedReader) for
/// length-prefixed framing. Datagram transports implement this trait directly.
#[async_trait]
pub trait PacketReader: Send + Sync + 'static {
    /// Stops transport background tasks before a disconnection event is queued.
    /// Implementations may retain their registration until this half is dropped.
    fn close(&mut self) {}

    /// Reads a single packet from this stream.
    ///
    /// `length_serializer` configures framing where the transport needs it;
    /// datagram transports may ignore it. Enforce `limits` before payload
    /// allocation or decoding.
    async fn receive<ReceivingPacket, SendingPacket, S, LS>(
        &mut self,
        serializer: Arc<S>,
        length_serializer: &LS,
        limits: &ReceiveLimits,
    ) -> Result<ReceivingPacket, ReceiveError<S::DecodeError, LS>>
    where
        ReceivingPacket: Send + Sync + Debug + 'static,
        SendingPacket: Send + Sync + Debug + 'static,
        S: Serializer<ReceivingPacket, SendingPacket> + ?Sized,
        LS: PacketLengthSerializer;

    /// Reads a packet and its receive time.
    ///
    /// Built-in protocols capture the time before decoding (and before UDP queueing).
    /// The default preserves custom `receive` implementations and timestamps their completion;
    /// override this method to provide the transport's receive time.
    async fn receive_with_timestamp<ReceivingPacket, SendingPacket, S, LS>(
        &mut self,
        serializer: Arc<S>,
        length_serializer: &LS,
        limits: &ReceiveLimits,
    ) -> Result<(ReceivingPacket, Instant), ReceiveError<S::DecodeError, LS>>
    where
        ReceivingPacket: Send + Sync + Debug + 'static,
        SendingPacket: Send + Sync + Debug + 'static,
        S: Serializer<ReceivingPacket, SendingPacket> + ?Sized,
        LS: PacketLengthSerializer,
    {
        let packet = self.receive(serializer, length_serializer, limits).await?;
        Ok((packet, Instant::now()))
    }
}

/// Sends packets using the transport's framing and delivery policy.
/// Byte transports can use [`FramedWriter`](super::protocol::FramedWriter).
#[async_trait]
pub trait PacketWriter: Send + Sync + 'static {
    /// Writes a packet to this stream.
    ///
    /// `length_serializer` configures framing where the transport needs it;
    /// datagram transports may ignore it.
    ///
    /// # Errors
    /// Reports encoding and fatal transport failures. A datagram transport may
    /// discard packets on recoverable send failures and report them through its
    /// diagnostics instead. Success does not imply remote delivery.
    async fn send<ReceivingPacket, SendingPacket, S, LS>(
        &mut self,
        packet: SendingPacket,
        serializer: Arc<S>,
        length_serializer: &LS,
    ) -> io::Result<()>
    where
        ReceivingPacket: Send + Sync + Debug + 'static,
        SendingPacket: Send + Sync + Debug + 'static,
        S: Serializer<ReceivingPacket, SendingPacket> + ?Sized,
        LS: PacketLengthSerializer;
}

/// Reports transport and decoding failures, including an explicitly closed connection.
#[derive(thiserror::Error)]
pub enum ReceiveError<SerializationError, LS>
where
    SerializationError: Error + Send + Sync,
    LS: PacketLengthSerializer,
{
    /// Stops reception when the transport cannot supply a complete packet.
    #[error("Failed to receive packet: {0}")]
    Io(#[source] io::Error),
    /// Rejects a complete payload using the packet codec's error.
    #[error("Failed to decode packet: {0}")]
    Deserialization(#[source] SerializationError),
    /// Stops framing because the prefix cannot be decoded.
    #[error("Failed to decode packet length: {0}")]
    LengthDeserialization(#[source] LS::Error),
    /// Exceeds the app-local [`MaxPacketSize`](crate::connection::MaxPacketSize).
    #[error("Packet exceeds the configured size limit")]
    PacketTooBig,
    /// Reports a failed connection attempt before packet reception starts.
    #[error("Failed to connect: {0}")]
    NoConnection(#[source] io::Error),
    /// Follows an explicit [`disconnect`](crate::connection::EcsConnection::disconnect) request.
    #[error("Connection was explicitly closed")]
    IntentionalDisconnection,
}

impl<SerializationError, LS> Debug for ReceiveError<SerializationError, LS>
where
    SerializationError: Error + Send + Sync,
    LS: PacketLengthSerializer,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "ReceiveError::Io({error:?})"),
            Self::Deserialization(error) => {
                write!(f, "ReceiveError::Deserialization({error:?})")
            }
            Self::LengthDeserialization(error) => {
                write!(f, "ReceiveError::LengthDeserialization({error:?})")
            }
            Self::PacketTooBig => write!(f, "ReceiveError::PacketTooBig"),
            Self::NoConnection(error) => write!(f, "ReceiveError::NoConnection({error:?})"),
            Self::IntentionalDisconnection => write!(f, "IntentionalDisconnection"),
        }
    }
}
