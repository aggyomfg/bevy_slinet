use core::fmt::Debug;
use std::{
    error::Error,
    sync::{Arc, Mutex},
};

/// Encodes and decodes packets shared by a connection’s read and write tasks.
pub trait Serializer<ReceivingPacket, SendingPacket>: Send + Sync + 'static
where
    ReceivingPacket: Send + Sync + Debug + 'static,
    SendingPacket: Send + Sync + Debug + 'static,
{
    /// Reports packets the codec cannot encode.
    type EncodeError: Error + Send + Sync;
    /// Reports bytes the codec cannot decode.
    type DecodeError: Error + Send + Sync;

    /// Consumes a packet for encoding.
    ///
    /// # Errors
    /// Returns the codec’s error when the packet cannot be represented.
    fn serialize(&self, packet: SendingPacket) -> Result<Vec<u8>, Self::EncodeError>;

    /// Decodes a complete packet payload.
    ///
    /// # Errors
    /// Returns the codec’s error for invalid or unsupported payloads.
    fn deserialize(&self, data: &[u8]) -> Result<ReceivingPacket, Self::DecodeError>;
}

/// Adapts a codec for shared access, locking mutable codecs for each operation.
/// A poisoned mutable codec panics on subsequent access.
pub enum SerializerAdapter<ReceivingPacket, SendingPacket, EncErr, DecErr>
where
    EncErr: Error + Send + Sync,
    DecErr: Error + Send + Sync,
{
    /// Allows independent calls without an adapter lock.
    ReadOnly(
        Arc<
            dyn ReadOnlySerializer<
                ReceivingPacket,
                SendingPacket,
                EncodeError = EncErr,
                DecodeError = DecErr,
            >,
        >,
    ),
    /// Serializes access to connection-local codec state.
    Mutable(
        Arc<
            Mutex<
                dyn MutableSerializer<
                    ReceivingPacket,
                    SendingPacket,
                    EncodeError = EncErr,
                    DecodeError = DecErr,
                >,
            >,
        >,
    ),
}

#[cfg(any(feature = "client", feature = "server"))]
impl<ReceivingPacket, SendingPacket, EncErr, DecErr>
    SerializerAdapter<ReceivingPacket, SendingPacket, EncErr, DecErr>
where
    EncErr: Error + Send + Sync,
    DecErr: Error + Send + Sync,
{
    pub(crate) fn warn_if_stateful_over_datagrams<P: crate::protocol::Protocol>(
        &self,
        warned: &mut bool,
    ) {
        if P::DATAGRAM && matches!(self, Self::Mutable(_)) && !std::mem::replace(warned, true) {
            bevy::log::warn!("A mutable serializer is used with a datagram protocol. Stateful serializers desynchronize when packets are lost or reordered.");
        }
    }
}

impl<ReceivingPacket, SendingPacket, EncErr, DecErr> Serializer<ReceivingPacket, SendingPacket>
    for SerializerAdapter<ReceivingPacket, SendingPacket, EncErr, DecErr>
where
    SendingPacket: Send + Sync + Debug + 'static,
    ReceivingPacket: Send + Sync + Debug + 'static,
    EncErr: Error + Send + Sync + 'static,
    DecErr: Error + Send + Sync + 'static,
{
    type EncodeError = EncErr;
    type DecodeError = DecErr;

    fn serialize(&self, packet: SendingPacket) -> Result<Vec<u8>, Self::EncodeError> {
        match self {
            SerializerAdapter::ReadOnly(serializer) => serializer.serialize(packet),
            SerializerAdapter::Mutable(serializer) => serializer.lock().unwrap().serialize(packet),
        }
    }

    fn deserialize(&self, data: &[u8]) -> Result<ReceivingPacket, Self::DecodeError> {
        match self {
            SerializerAdapter::ReadOnly(serializer) => serializer.deserialize(data),
            SerializerAdapter::Mutable(serializer) => serializer.lock().unwrap().deserialize(data),
        }
    }
}

/// Supports shared access without mutable codec state.
pub trait ReadOnlySerializer<ReceivingPacket, SendingPacket>: Send + Sync + 'static {
    /// Reports packets the codec cannot encode.
    type EncodeError: Error + Send + Sync;
    /// Reports bytes the codec cannot decode.
    type DecodeError: Error + Send + Sync;
    /// Consumes a packet for encoding.
    ///
    /// # Errors
    /// Returns the codec’s error when the packet cannot be represented.
    fn serialize(&self, packet: SendingPacket) -> Result<Vec<u8>, Self::EncodeError>;
    /// Decodes a complete packet payload.
    ///
    /// # Errors
    /// Returns the codec’s error for invalid or unsupported payloads.
    fn deserialize(&self, buffer: &[u8]) -> Result<ReceivingPacket, Self::DecodeError>;
}
/// Maintains connection-local codec state; datagram codecs must tolerate loss and reordering.
pub trait MutableSerializer<ReceivingPacket, SendingPacket>: Send + Sync + 'static {
    /// Reports packets the codec cannot encode.
    type EncodeError: Error + Send + Sync;
    /// Reports bytes the codec cannot decode.
    type DecodeError: Error + Send + Sync;
    /// Consumes a packet for encoding.
    ///
    /// # Errors
    /// Returns the codec’s error when the packet cannot be represented.
    fn serialize(&mut self, packet: SendingPacket) -> Result<Vec<u8>, Self::EncodeError>;
    /// Decodes a complete packet payload.
    ///
    /// # Errors
    /// Returns the codec’s error for invalid or unsupported payloads.
    fn deserialize(&mut self, buffer: &[u8]) -> Result<ReceivingPacket, Self::DecodeError>;
}
