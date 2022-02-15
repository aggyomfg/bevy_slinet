//! A [`bincode`]-based packet serializer. You can enable it by adding `serializer_bincode` feature.

use crate::serializer::Serializer;
pub use bincode::{DefaultOptions, Options};
use serde::{Deserialize, Serialize};

/// See [`bincode`] and [`bincode::Options`] for details.
#[derive(Default, Clone)]
pub struct BincodeSerializer<O>(pub O)
where
    O: Options + Send + Sync + 'static;

impl<ReceivingPacket, SendingPacket, O> Serializer<ReceivingPacket, SendingPacket>
    for BincodeSerializer<O>
where
    ReceivingPacket: for<'de> Deserialize<'de>,
    SendingPacket: Serialize,
    O: Options + Clone + Send + Sync + 'static,
{
    type Error = bincode::Error;

    fn serialize(&self, t: SendingPacket) -> Result<Vec<u8>, Self::Error> {
        self.0.clone().serialize(&t)
    }

    fn deserialize(&self, bytes: &[u8]) -> Result<ReceivingPacket, Self::Error> {
        self.0.clone().deserialize(bytes)
    }
}
