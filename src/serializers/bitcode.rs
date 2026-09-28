//! A [`bitcode`]-based packet serializer using native bitcode traits.

use std::convert::Infallible;

use crate::serializers::serializer::ReadOnlySerializer;

/// Encodes packets using native [`bitcode::Encode`]/[`bitcode::Decode`] traits.
#[derive(Clone, Default)]
pub struct BitcodeSerializer;

impl BitcodeSerializer {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl<ReceivingPacket, SendingPacket> ReadOnlySerializer<ReceivingPacket, SendingPacket>
    for BitcodeSerializer
where
    ReceivingPacket: bitcode::DecodeOwned,
    SendingPacket: bitcode::Encode,
{
    type EncodeError = Infallible;
    type DecodeError = bitcode::Error;

    fn serialize(&self, packet: SendingPacket) -> Result<Vec<u8>, Self::EncodeError> {
        Ok(bitcode::encode(&packet))
    }

    fn deserialize(&self, bytes: &[u8]) -> Result<ReceivingPacket, Self::DecodeError> {
        bitcode::decode(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, bitcode::Decode, bitcode::Encode, PartialEq)]
    struct TestPacket {
        id: u32,
        message: String,
    }

    #[derive(Clone, Debug, bitcode::Decode, bitcode::Encode, PartialEq)]
    enum TestEnum {
        Ping,
        Move { x: f32, y: f32 },
        Chat(String),
    }

    fn roundtrip<T>(packet: T) -> T
    where
        T: bitcode::Encode + bitcode::DecodeOwned + Clone,
    {
        let serializer = BitcodeSerializer::new();
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

    #[cfg(all(feature = "client", feature = "server", feature = "protocol_tcp"))]
    #[test]
    fn test_tcp_echo() {
        use crate::client::{self, ClientPlugin, ConnectionEstablishEvent};
        use crate::protocols::tcp::TcpProtocol;
        use crate::serializers::packet_length_serializer::LittleEndian;
        use crate::serializers::serializer::SerializerAdapter;
        use crate::server::{self, ServerAddress, ServerPlugin};
        use crate::{ClientConfig, ServerConfig};
        use bevy::prelude::*;
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        struct Config;

        impl ServerConfig for Config {
            type ClientPacket = TestEnum;
            type ServerPacket = TestEnum;
            type Protocol = TcpProtocol;
            type EncodeError = Infallible;
            type DecodeError = bitcode::Error;
            type LengthSerializer = LittleEndian<u32>;
            fn build_serializer(
            ) -> SerializerAdapter<TestEnum, TestEnum, Infallible, bitcode::Error> {
                SerializerAdapter::ReadOnly(Arc::new(BitcodeSerializer))
            }
        }

        impl ClientConfig for Config {
            type ClientPacket = TestEnum;
            type ServerPacket = TestEnum;
            type Protocol = TcpProtocol;
            type EncodeError = Infallible;
            type DecodeError = bitcode::Error;
            type LengthSerializer = LittleEndian<u32>;
            fn build_serializer(
            ) -> SerializerAdapter<TestEnum, TestEnum, Infallible, bitcode::Error> {
                SerializerAdapter::ReadOnly(Arc::new(BitcodeSerializer))
            }
        }

        #[derive(Resource, Default)]
        struct Received(Option<TestEnum>);

        let packet = TestEnum::Move { x: 1.5, y: -2.0 };

        let mut app_server = App::new();
        app_server.add_plugins(ServerPlugin::<Config>::bind("127.0.0.1:0"));
        app_server.add_observer(|event: On<server::PacketReceiveEvent<Config>>| {
            let event = event.event();
            event.connection.send(event.packet.clone()).unwrap();
        });
        app_server.update();
        let server_addr = app_server
            .world()
            .resource::<ServerAddress<Config>>()
            .address();

        let mut app_client = App::new();
        app_client.add_plugins(ClientPlugin::<Config>::connect(server_addr));
        app_client.init_resource::<Received>();
        let sent = packet.clone();
        app_client.add_observer(move |event: On<ConnectionEstablishEvent<Config>>| {
            event.event().connection.send(sent.clone()).unwrap();
        });
        app_client.add_observer(
            |event: On<client::PacketReceiveEvent<Config>>, mut received: ResMut<Received>| {
                received.0 = Some(event.event().packet.clone());
            },
        );

        let deadline = Instant::now() + Duration::from_secs(5);
        while app_client.world().resource::<Received>().0.is_none() && Instant::now() < deadline {
            app_client.update();
            app_server.update();
            std::thread::sleep(Duration::from_millis(10));
        }

        assert_eq!(app_client.world().resource::<Received>().0, Some(packet));
    }

    #[test]
    fn test_invalid_bytes() {
        let serializer = BitcodeSerializer::new();
        let result =
            ReadOnlySerializer::<TestPacket, TestPacket>::deserialize(&serializer, &[0xFF]);
        assert!(result.is_err());
    }
}
