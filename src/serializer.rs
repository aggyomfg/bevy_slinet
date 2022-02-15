//! Implement [`Serializer`] trait or use built-in serializers listed in the [`serializers`](crate::serializers) module.

use std::error::Error;

/// A packet serializer
pub trait Serializer<ReceivingPacket, SendingPacket>: Send + Sync + 'static {
    /// A serializer's error that may be returned from [`Self::deserialize`].
    type Error: Error + Send + Sync;

    /// Serialize a packet
    fn serialize(&self, t: SendingPacket) -> Result<Vec<u8>, Self::Error>;
    /// Deserialize a packet
    fn deserialize(&self, buffer: &[u8]) -> Result<ReceivingPacket, Self::Error>;
}
