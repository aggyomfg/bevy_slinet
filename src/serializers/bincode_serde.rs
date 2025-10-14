//! A [`bincode`]-based packet serializer with serde support.

use crate::serializer::ReadOnlySerializer;
pub use bincode::config;
use bincode::config::Configuration;
use serde::{Deserialize, Serialize};

/// Bincode 2.x serializer using serde traits and standard configuration.
/// Provides compatibility with other serde-based formats.
///
/// For better performance without serde overhead, use `BincodeSerializer` instead.
#[derive(Clone)]
pub struct BincodeSerdeSerializer {
    config: Configuration,
}

impl BincodeSerdeSerializer {
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

    fn serialize(&self, t: SendingPacket) -> Result<Vec<u8>, Self::EncodeError> {
        bincode::serde::encode_to_vec(&t, self.config)
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
        let serializer = BincodeSerdeSerializer::default();
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
        let serializer = BincodeSerdeSerializer::default();
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
