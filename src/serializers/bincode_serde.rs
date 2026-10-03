//! A [`bincode`]-based packet serializer with serde support.
//!
//! Kept for compatibility only: bincode is unmaintained upstream. Prefer `serializer_bitcode_serde`.

use crate::serializers::serializer::ReadOnlySerializer;
pub use bincode::config;
use bincode::config::Configuration;
use serde::{Deserialize, Serialize};

/// Bincode 2.x serializer using serde traits and standard configuration.
///
/// For new code, use `BitcodeSerdeSerializer` instead.
#[derive(Clone)]
pub struct BincodeSerdeSerializer {
    config: Configuration,
}

impl BincodeSerdeSerializer {
    /// Creates a serializer with its default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for BincodeSerdeSerializer {
    fn default() -> Self {
        Self {
            config: bincode::config::standard(),
        }
    }
}

impl<ReceivingPacket, SendingPacket> ReadOnlySerializer<ReceivingPacket, SendingPacket>
    for BincodeSerdeSerializer
where
    ReceivingPacket: for<'de> Deserialize<'de>,
    SendingPacket: Serialize,
{
    type EncodeError = bincode::error::EncodeError;
    type DecodeError = bincode::error::DecodeError;

    fn serialize(&self, packet: SendingPacket) -> Result<Vec<u8>, Self::EncodeError> {
        bincode::serde::encode_to_vec(&packet, self.config)
    }

    fn deserialize(&self, bytes: &[u8]) -> Result<ReceivingPacket, Self::DecodeError> {
        bincode::serde::decode_from_slice(bytes, self.config).map(|(packet, _len)| packet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
    struct TestPacket {
        id: u32,
        message: String,
    }

    #[test]
    fn test_serialize_deserialize_default() {
        let serializer = BincodeSerdeSerializer::default();
        let packet = TestPacket {
            id: 42,
            message: "Hello".to_string(),
        };

        let encoded =
            ReadOnlySerializer::<TestPacket, TestPacket>::serialize(&serializer, packet.clone())
                .unwrap();
        let deserialized: TestPacket =
            ReadOnlySerializer::<TestPacket, TestPacket>::deserialize(&serializer, &encoded)
                .unwrap();

        assert_eq!(packet, deserialized);
    }

    #[test]
    fn test_large_packet() {
        let serializer = BincodeSerdeSerializer::default();
        let packet = TestPacket {
            id: 999_999,
            message: "A".repeat(10000),
        };

        let encoded =
            ReadOnlySerializer::<TestPacket, TestPacket>::serialize(&serializer, packet.clone())
                .unwrap();
        let deserialized: TestPacket =
            ReadOnlySerializer::<TestPacket, TestPacket>::deserialize(&serializer, &encoded)
                .unwrap();

        assert_eq!(packet, deserialized);
    }

    #[test]
    fn test_empty_string() {
        let serializer = BincodeSerdeSerializer::default();
        let packet = TestPacket {
            id: 0,
            message: String::new(),
        };

        let encoded =
            ReadOnlySerializer::<TestPacket, TestPacket>::serialize(&serializer, packet.clone())
                .unwrap();
        let deserialized: TestPacket =
            ReadOnlySerializer::<TestPacket, TestPacket>::deserialize(&serializer, &encoded)
                .unwrap();

        assert_eq!(packet, deserialized);
    }
}
