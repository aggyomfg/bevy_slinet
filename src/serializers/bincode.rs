//! A [`bincode`]-based packet serializer using native bincode traits.
//!
//! Kept for compatibility only: bincode is unmaintained upstream. Prefer `serializer_bitcode`.

use crate::serializer::ReadOnlySerializer;
pub use bincode::config;
use bincode::config::Configuration;

/// Bincode 2.x serializer using native Encode/Decode traits.
///
/// For new code, use `BitcodeSerializer` instead.
#[derive(Clone)]
pub struct BincodeSerializer {
    config: Configuration,
}

impl BincodeSerializer {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for BincodeSerializer {
    fn default() -> Self {
        Self {
            config: bincode::config::standard(),
        }
    }
}

impl<ReceivingPacket, SendingPacket> ReadOnlySerializer<ReceivingPacket, SendingPacket>
    for BincodeSerializer
where
    ReceivingPacket: bincode::Decode<()>,
    SendingPacket: bincode::Encode,
{
    type EncodeError = bincode::error::EncodeError;
    type DecodeError = bincode::error::DecodeError;

    fn serialize(&self, t: SendingPacket) -> Result<Vec<u8>, Self::EncodeError> {
        bincode::encode_to_vec(&t, self.config)
    }

    fn deserialize(&self, bytes: &[u8]) -> Result<ReceivingPacket, Self::DecodeError> {
        bincode::decode_from_slice(bytes, self.config).map(|(packet, _len)| packet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, bincode::Decode, bincode::Encode, PartialEq)]
    struct TestPacket {
        id: u32,
        message: String,
    }

    #[test]
    fn test_serialize_deserialize_default() {
        let serializer = BincodeSerializer::default();
        let packet = TestPacket {
            id: 42,
            message: "Hello".to_string(),
        };

        let serialized =
            ReadOnlySerializer::<TestPacket, TestPacket>::serialize(&serializer, packet.clone())
                .unwrap();
        let deserialized: TestPacket =
            ReadOnlySerializer::<TestPacket, TestPacket>::deserialize(&serializer, &serialized)
                .unwrap();

        assert_eq!(packet, deserialized);
    }

    #[test]
    fn test_large_packet() {
        let serializer = BincodeSerializer::default();
        let packet = TestPacket {
            id: 999999,
            message: "A".repeat(10000),
        };

        let serialized =
            ReadOnlySerializer::<TestPacket, TestPacket>::serialize(&serializer, packet.clone())
                .unwrap();
        let deserialized: TestPacket =
            ReadOnlySerializer::<TestPacket, TestPacket>::deserialize(&serializer, &serialized)
                .unwrap();

        assert_eq!(packet, deserialized);
    }

    #[test]
    fn test_empty_string() {
        let serializer = BincodeSerializer::default();
        let packet = TestPacket {
            id: 0,
            message: String::new(),
        };

        let serialized =
            ReadOnlySerializer::<TestPacket, TestPacket>::serialize(&serializer, packet.clone())
                .unwrap();
        let deserialized: TestPacket =
            ReadOnlySerializer::<TestPacket, TestPacket>::deserialize(&serializer, &serialized)
                .unwrap();

        assert_eq!(packet, deserialized);
    }
}
