//! A [`bitcode`]-based packet serializer with serde support.

use crate::serializer::ReadOnlySerializer;
use serde::{Deserialize, Serialize};

/// Encodes packets through [`serde`] using the bitcode format.
#[derive(Clone, Default)]
pub struct BitcodeSerdeSerializer;

impl BitcodeSerdeSerializer {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl<ReceivingPacket, SendingPacket> ReadOnlySerializer<ReceivingPacket, SendingPacket>
    for BitcodeSerdeSerializer
where
    ReceivingPacket: for<'de> Deserialize<'de>,
    SendingPacket: Serialize,
{
    type EncodeError = bitcode::Error;
    type DecodeError = bitcode::Error;

    fn serialize(&self, packet: SendingPacket) -> Result<Vec<u8>, Self::EncodeError> {
        bitcode::serialize(&packet)
    }

    fn deserialize(&self, bytes: &[u8]) -> Result<ReceivingPacket, Self::DecodeError> {
        bitcode::deserialize(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
    struct TestPacket {
        id: u32,
        message: String,
    }

    #[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
    enum TestEnum {
        Ping,
        Move { x: f32, y: f32 },
        Chat(String),
    }

    fn roundtrip<T>(packet: T) -> T
    where
        T: Serialize + for<'de> Deserialize<'de> + Clone,
    {
        let serializer = BitcodeSerdeSerializer::new();
        let encoded = ReadOnlySerializer::<T, T>::serialize(&serializer, packet).unwrap();
        ReadOnlySerializer::<T, T>::deserialize(&serializer, &encoded).unwrap()
    }

    #[test]
    fn test_serialize_deserialize_default() {
        let packet = TestPacket {
            id: 42,
            message: "Hello".to_string(),
        };
        assert_eq!(roundtrip(packet.clone()), packet);
    }

    #[test]
    fn test_large_packet() {
        let packet = TestPacket {
            id: 999_999,
            message: "A".repeat(10000),
        };
        assert_eq!(roundtrip(packet.clone()), packet);
    }

    #[test]
    fn test_empty_string() {
        let packet = TestPacket {
            id: 0,
            message: String::new(),
        };
        assert_eq!(roundtrip(packet.clone()), packet);
    }

    #[test]
    fn test_enum() {
        for packet in [
            TestEnum::Ping,
            TestEnum::Move { x: 1.5, y: -2.0 },
            TestEnum::Chat("hi".to_string()),
        ] {
            assert_eq!(roundtrip(packet.clone()), packet);
        }
    }

    #[test]
    fn test_invalid_bytes() {
        let serializer = BitcodeSerdeSerializer::new();
        let result =
            ReadOnlySerializer::<TestPacket, TestPacket>::deserialize(&serializer, &[0xFF]);
        assert!(result.is_err());
    }
}
