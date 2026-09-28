#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "Test helpers and Bevy test systems fail the test on errors"
)]
use bevy::app::App;
use bevy::prelude::*;

use crate::client::{self, ClientConnection, ClientPlugin, ConnectionEstablishEvent};
use crate::packet_length_serializer::LittleEndian;
use crate::protocols::tcp::TcpProtocol;
use crate::serializer::SerializerAdapter;
use crate::serializers::custom_crypt::{
    CustomCryptClientPacket, CustomCryptEngine, CustomCryptSerializer, CustomCryptServerPacket,
    CustomSerializationError,
};
use crate::server::{self, NewConnectionEvent, ServerAddress, ServerConnections, ServerPlugin};
use crate::tests::wait_until;
use crate::{ClientConfig, ServerConfig};

use std::sync::{Arc, Mutex};

struct TcpConfig;

impl ServerConfig for TcpConfig {
    type ClientPacket = CustomCryptClientPacket;
    type ServerPacket = CustomCryptServerPacket;
    type Protocol = TcpProtocol;

    type EncodeError = CustomSerializationError;
    type DecodeError = CustomSerializationError;

    type LengthSerializer = LittleEndian<u32>;

    fn build_serializer() -> SerializerAdapter<
        Self::ClientPacket,
        Self::ServerPacket,
        Self::EncodeError,
        Self::DecodeError,
    > {
        SerializerAdapter::Mutable(Arc::new(Mutex::new(CustomCryptSerializer::<
            CustomCryptEngine,
            Self::ClientPacket,
            Self::ServerPacket,
        >::new(
            CustomCryptEngine::default()
        ))))
    }
}

impl ClientConfig for TcpConfig {
    type ClientPacket = CustomCryptClientPacket;
    type ServerPacket = CustomCryptServerPacket;
    type Protocol = TcpProtocol;
    type EncodeError = CustomSerializationError;
    type DecodeError = CustomSerializationError;

    type LengthSerializer = LittleEndian<u32>;
    fn build_serializer() -> SerializerAdapter<
        Self::ServerPacket,
        Self::ClientPacket,
        Self::EncodeError,
        Self::DecodeError,
    > {
        SerializerAdapter::Mutable(Arc::new(Mutex::new(CustomCryptSerializer::<
            CustomCryptEngine,
            Self::ServerPacket,
            Self::ClientPacket,
        >::new(
            CustomCryptEngine::default()
        ))))
    }
}

#[test]
fn tcp_connection() {
    let mut app_server = App::new();
    app_server.add_plugins(ServerPlugin::<TcpConfig>::bind("127.0.0.1:0"));
    app_server.update(); // bind
    let srv_addr = app_server
        .world()
        .resource::<ServerAddress<TcpConfig>>()
        .address();

    let mut app_client = App::new();
    app_client.add_plugins(ClientPlugin::<TcpConfig>::connect(srv_addr));

    wait_until(|| {
        app_client.update();
        app_server.update();
        app_client
            .world()
            .contains_resource::<ClientConnection<TcpConfig>>()
            && app_server
                .world()
                .resource::<ServerConnections<TcpConfig>>()
                .len()
                == 1
    });

    assert!(
        app_client
            .world()
            .get_resource::<ClientConnection<TcpConfig>>()
            .is_some(),
        "No ClientConnection resource found"
    );
    assert_eq!(
        app_server
            .world()
            .get_resource::<ServerConnections<TcpConfig>>()
            .unwrap()
            .len(),
        1,
    );
}

#[derive(Default, Resource)]
struct ReceivedPackets<T> {
    packets: Vec<T>,
}

#[derive(Resource)]
struct ClientToServerPacketResource(CustomCryptClientPacket);

#[derive(Resource)]
struct ServerToClientPacketResource(CustomCryptServerPacket);

#[test]
fn tcp_encrypted_packets() {
    let client_to_server_packet = CustomCryptClientPacket::String("Hello, Server!".to_string());
    let server_to_client_packet = CustomCryptServerPacket::String("Hello, Client!".to_string());

    let mut app_server = App::new();
    app_server.add_plugins(ServerPlugin::<TcpConfig>::bind("127.0.0.1:0"));
    app_server.insert_resource(ReceivedPackets::<CustomCryptClientPacket>::default());
    app_server.insert_resource(ServerToClientPacketResource(
        server_to_client_packet.clone(),
    ));

    app_server.add_observer(server_new_connection_system);
    app_server.add_observer(server_packet_receive_system);

    app_server.update(); // bind
    let server_addr = app_server
        .world()
        .resource::<ServerAddress<TcpConfig>>()
        .address();

    let mut app_client = App::new();
    app_client.add_plugins(ClientPlugin::<TcpConfig>::connect(server_addr));
    app_client.insert_resource(ReceivedPackets::<CustomCryptServerPacket>::default());
    app_client.insert_resource(ClientToServerPacketResource(
        client_to_server_packet.clone(),
    ));

    app_client.add_observer(client_connection_establish_system);
    app_client.add_observer(client_packet_receive_system);

    wait_until(|| {
        app_client.update();
        app_server.update();
        !app_server
            .world()
            .resource::<ReceivedPackets<CustomCryptClientPacket>>()
            .packets
            .is_empty()
            && !app_client
                .world()
                .resource::<ReceivedPackets<CustomCryptServerPacket>>()
                .packets
                .is_empty()
    });

    // Check if the server received the packet from the client
    let server_received_packets = app_server
        .world()
        .get_resource::<ReceivedPackets<CustomCryptClientPacket>>()
        .unwrap();
    assert_eq!(
        server_received_packets.packets.first(),
        Some(&client_to_server_packet),
        "Server did not receive the expected packet from client"
    );

    // Check if the client received the packet from the server
    let client_received_packets = app_client
        .world()
        .get_resource::<ReceivedPackets<CustomCryptServerPacket>>()
        .unwrap();
    assert_eq!(
        client_received_packets.packets.first(),
        Some(&server_to_client_packet),
        "Client did not receive the expected packet from server"
    );
}

fn server_new_connection_system(
    event: On<NewConnectionEvent<TcpConfig>>,
    server_to_client_packet: Res<ServerToClientPacketResource>,
) {
    event
        .event()
        .connection
        .send(server_to_client_packet.0.clone())
        .expect("Couldn't send server packet");
}

fn server_packet_receive_system(
    event: On<server::PacketReceiveEvent<TcpConfig>>,
    mut received_packets: ResMut<ReceivedPackets<CustomCryptClientPacket>>,
) {
    received_packets.packets.push(event.event().packet.clone());
}

fn client_connection_establish_system(
    event: On<ConnectionEstablishEvent<TcpConfig>>,
    client_to_server_packet: Res<ClientToServerPacketResource>,
) {
    event
        .event()
        .connection
        .send(client_to_server_packet.0.clone())
        .expect("Couldn't send client packet");
}

fn client_packet_receive_system(
    event: On<client::PacketReceiveEvent<TcpConfig>>,
    mut received_packets: ResMut<ReceivedPackets<CustomCryptServerPacket>>,
) {
    received_packets.packets.push(event.event().packet.clone());
}
