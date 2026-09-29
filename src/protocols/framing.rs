//! Length-prefix framing over byte streams.

use async_trait::async_trait;
use bevy::platform::time::Instant;
use std::error::Error;
use std::fmt::Debug;
use std::io;
use std::sync::Arc;

use super::packet::{PacketReader, PacketWriter, ReceiveError};
use crate::connection::ReceiveLimits;
use crate::serializers::packet_length_serializer::PacketLengthDeserializationError;
use crate::serializers::serializer::Serializer;
use crate::PacketLengthSerializer;

/// Reads bytes from a continuous stream, without packet framing.
#[async_trait]
pub trait ReadStream: Send + Sync + 'static {
    /// Fills the whole buffer, or returns a transport error.
    async fn read_exact(&mut self, buffer: &mut [u8]) -> io::Result<()>;
}

/// Writes bytes to a continuous stream, without packet framing.
#[async_trait]
pub trait WriteStream: Send + Sync + 'static {
    /// Writes the whole buffer, or returns a transport error.
    async fn write_all(&mut self, buffer: &[u8]) -> io::Result<()>;
}

/// Reads length-prefixed packets from a byte stream.
///
/// The adapter uses the default no-op packet lifecycle hooks. Transports needing
/// explicit shutdown or idle-timeout updates should implement [`PacketReader`]
/// on their own wrapper, delegate framing here, and handle those hooks themselves.
#[derive(Debug)]
pub struct FramedReader<R> {
    inner: R,
}

impl<R> FramedReader<R> {
    /// Wraps a byte stream with length-prefix framing.
    #[must_use]
    pub const fn new(inner: R) -> Self {
        Self { inner }
    }

    /// Borrows the underlying byte stream.
    #[must_use]
    pub const fn get_ref(&self) -> &R {
        &self.inner
    }

    /// Mutably borrows the underlying byte stream.
    #[must_use]
    pub const fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    /// Removes the framing adapter and returns the byte stream.
    #[must_use]
    pub fn into_inner(self) -> R {
        self.inner
    }
}

/// Writes length-prefixed packets to a byte stream.
#[derive(Debug)]
pub struct FramedWriter<W> {
    inner: W,
}

impl<W> FramedWriter<W> {
    /// Wraps a byte stream with length-prefix framing.
    #[must_use]
    pub const fn new(inner: W) -> Self {
        Self { inner }
    }

    /// Borrows the underlying byte stream.
    #[must_use]
    pub const fn get_ref(&self) -> &W {
        &self.inner
    }

    /// Mutably borrows the underlying byte stream.
    #[must_use]
    pub const fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }

    /// Removes the framing adapter and returns the byte stream.
    #[must_use]
    pub fn into_inner(self) -> W {
        self.inner
    }
}

#[async_trait]
impl<R: ReadStream> PacketReader for FramedReader<R> {
    async fn receive<ReceivingPacket, SendingPacket, S, LS>(
        &mut self,
        serializer: Arc<S>,
        length_serializer: &LS,
        limits: &ReceiveLimits,
    ) -> Result<ReceivingPacket, ReceiveError<S::DecodeError, LS::Error>>
    where
        ReceivingPacket: Send + Sync + Debug + 'static,
        SendingPacket: Send + Sync + Debug + 'static,
        S: Serializer<ReceivingPacket, SendingPacket> + ?Sized,
        LS: PacketLengthSerializer,
    {
        self.receive_with_timestamp(serializer, length_serializer, limits)
            .await
            .map(|(packet, _)| packet)
    }

    async fn receive_with_timestamp<ReceivingPacket, SendingPacket, S, LS>(
        &mut self,
        serializer: Arc<S>,
        length_serializer: &LS,
        limits: &ReceiveLimits,
    ) -> Result<(ReceivingPacket, Instant), ReceiveError<S::DecodeError, LS::Error>>
    where
        ReceivingPacket: Send + Sync + Debug + 'static,
        SendingPacket: Send + Sync + Debug + 'static,
        S: Serializer<ReceivingPacket, SendingPacket> + ?Sized,
        LS: PacketLengthSerializer,
    {
        let length = self.read_length(length_serializer).await?;
        if length > limits.max_packet_size() {
            return Err(ReceiveError::PacketTooBig);
        }

        let mut payload = vec![0; length];
        self.inner
            .read_exact(&mut payload)
            .await
            .map_err(ReceiveError::Io)?;
        let received_at = Instant::now();
        let packet = serializer
            .deserialize(&payload)
            .map_err(ReceiveError::Deserialization)?;
        Ok((packet, received_at))
    }
}

impl<R: ReadStream> FramedReader<R> {
    async fn read_length<DecErr, LS>(
        &mut self,
        length_serializer: &LS,
    ) -> Result<usize, ReceiveError<DecErr, LS::Error>>
    where
        DecErr: Error + Send + Sync,
        LS: PacketLengthSerializer,
    {
        let mut prefix = vec![0; LS::SIZE];
        let mut filled = 0;
        loop {
            self.inner
                .read_exact(
                    prefix.get_mut(filled..).ok_or_else(|| {
                        ReceiveError::Io(io::Error::from(io::ErrorKind::InvalidData))
                    })?,
                )
                .await
                .map_err(ReceiveError::Io)?;
            match length_serializer.deserialize_packet_length(&prefix) {
                Ok(length) => return Ok(length),
                Err(PacketLengthDeserializationError::NeedMoreBytes(additional)) => {
                    filled = prefix.len();
                    prefix.resize(filled + additional, 0);
                }
                Err(PacketLengthDeserializationError::Err(error)) => {
                    return Err(ReceiveError::LengthDeserialization(error));
                }
            }
        }
    }
}

#[async_trait]
impl<W: WriteStream> PacketWriter for FramedWriter<W> {
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
        LS: PacketLengthSerializer,
    {
        let payload = serializer
            .serialize(packet)
            .map_err(|err| io::Error::other(format!("Error serializing packet: {err}")))?;
        let mut buf = length_serializer
            .serialize_packet_length(payload.len())
            .map_err(|err| io::Error::other(format!("Error serializing packet length: {err}")))?;
        buf.extend_from_slice(&payload);
        self.inner.write_all(&buf).await
    }
}
