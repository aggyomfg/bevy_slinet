#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "Test helpers and Bevy test systems fail the test on errors"
)]
use crate::client;
use crate::client::{ClientConnection, ClientPlugin, ConnectionEstablishEvent};
use crate::packet_length_serializer::LittleEndian;
use crate::serializer::SerializerAdapter;
use crate::serializers::bitcode_serde::BitcodeSerdeSerializer;
use crate::server::{NewConnectionEvent, ServerAddress, ServerConnections, ServerPlugin};
use crate::{server, ClientConfig, ServerConfig};
use bevy::app::App;
use bevy::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Calls `step` until it returns `true` or a timeout expires.
pub fn wait_until(mut step: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !step() {
        assert!(
            Instant::now() < deadline,
            "condition was not met before timeout"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
struct Packet(u64);

macro_rules! test_config {
    ($name:ident, $protocol:ty) => {
        struct $name;

        impl ServerConfig for $name {
            type ClientPacket = Packet;
            type ServerPacket = Packet;
            type Protocol = $protocol;

            type EncodeError = bitcode::Error;
            type DecodeError = bitcode::Error;

            type LengthSerializer = LittleEndian<u32>;

            fn build_serializer() -> SerializerAdapter<
                Self::ClientPacket,
                Self::ServerPacket,
                Self::EncodeError,
                Self::DecodeError,
            > {
                SerializerAdapter::ReadOnly(Arc::new(BitcodeSerdeSerializer))
            }
        }

        impl ClientConfig for $name {
            type ClientPacket = Packet;
            type ServerPacket = Packet;
            type Protocol = $protocol;
            type EncodeError = bitcode::Error;
            type DecodeError = bitcode::Error;

            type LengthSerializer = LittleEndian<u32>;
            fn build_serializer() -> SerializerAdapter<
                Self::ServerPacket,
                Self::ClientPacket,
                Self::EncodeError,
                Self::DecodeError,
            > {
                SerializerAdapter::ReadOnly(Arc::new(BitcodeSerdeSerializer))
            }
        }
    };
}

#[cfg(all(feature = "protocol_tcp", feature = "serializer_bitcode"))]
mod mut_serializer;
#[cfg(feature = "protocol_tcp")]
mod tcp;
#[cfg(feature = "protocol_udp")]
mod udp;

trait TestConfig:
    ServerConfig<ClientPacket = Packet, ServerPacket = Packet>
    + ClientConfig<ClientPacket = Packet, ServerPacket = Packet>
{
}

impl<C> TestConfig for C where
    C: ServerConfig<ClientPacket = Packet, ServerPacket = Packet>
        + ClientConfig<ClientPacket = Packet, ServerPacket = Packet>
{
}

#[derive(Default, Resource)]
struct ReceivedPackets<T> {
    packets: Vec<T>,
}

#[derive(Resource)]
struct ClientToServerPacketResource(Packet);

#[derive(Resource)]
struct ServerToClientPacketResource(Packet);

fn exchange_packets<C: TestConfig>() -> (App, App) {
    let client_to_server_packet = Packet(42);
    let server_to_client_packet = Packet(24);

    let mut app_server = App::new();
    app_server.add_plugins(ServerPlugin::<C>::bind("127.0.0.1:0"));
    app_server.insert_resource(ReceivedPackets::<Packet>::default());
    app_server.insert_resource(ServerToClientPacketResource(server_to_client_packet));

    app_server.add_observer(server_new_connection_system::<C>);
    app_server.add_observer(server_packet_receive_system::<C>);

    app_server.update(); // bind
    let server_addr = app_server.world().resource::<ServerAddress<C>>().address();

    let mut app_client = App::new();
    app_client.add_plugins(ClientPlugin::<C>::connect(server_addr));
    app_client.insert_resource(ReceivedPackets::<Packet>::default());
    app_client.insert_resource(ClientToServerPacketResource(client_to_server_packet));

    app_client.add_observer(client_connection_establish_system::<C>);
    app_client.add_observer(client_packet_receive_system::<C>);

    wait_until(|| {
        app_client.update();
        app_server.update();
        !app_server
            .world()
            .resource::<ReceivedPackets<Packet>>()
            .packets
            .is_empty()
            && !app_client
                .world()
                .resource::<ReceivedPackets<Packet>>()
                .packets
                .is_empty()
    });

    // Check if the server received the packet from the client
    let server_received_packets = app_server
        .world()
        .get_resource::<ReceivedPackets<Packet>>()
        .unwrap();
    assert_eq!(
        server_received_packets.packets.first(),
        Some(&client_to_server_packet),
        "Server did not receive the expected packet from client"
    );

    // Check if the client received the packet from the server
    let client_received_packets = app_client
        .world()
        .get_resource::<ReceivedPackets<Packet>>()
        .unwrap();
    assert_eq!(
        client_received_packets.packets.first(),
        Some(&server_to_client_packet),
        "Client did not receive the expected packet from server"
    );
    (app_server, app_client)
}

fn server_new_connection_system<C: TestConfig>(
    event: On<NewConnectionEvent<C>>,
    server_to_client_packet: Res<ServerToClientPacketResource>,
) {
    event
        .event()
        .connection
        .send(server_to_client_packet.0)
        .expect("Couldn't send server packet");
}

fn server_packet_receive_system<C: TestConfig>(
    event: On<server::PacketReceiveEvent<C>>,
    mut received_packets: ResMut<ReceivedPackets<Packet>>,
) {
    assert!(event.event().received_at <= bevy::platform::time::Instant::now());
    received_packets.packets.push(event.event().packet);
}

fn client_connection_establish_system<C: TestConfig>(
    event: On<ConnectionEstablishEvent<C>>,
    client_to_server_packet: Res<ClientToServerPacketResource>,
) {
    event
        .event()
        .connection
        .send(client_to_server_packet.0)
        .expect("Couldn't send client packet");
}

fn client_packet_receive_system<C: TestConfig>(
    event: On<client::PacketReceiveEvent<C>>,
    mut received_packets: ResMut<ReceivedPackets<Packet>>,
) {
    assert!(event.event().received_at <= bevy::platform::time::Instant::now());
    received_packets.packets.push(event.event().packet);
}
