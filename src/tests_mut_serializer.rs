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
use crate::server::{self, NewConnectionEvent, ServerConnections, ServerPlugin};
use crate::{ClientConfig, ServerConfig};

use std::sync::{Arc, Mutex};
use std::time::Duration;

struct TcpConfig;

impl ServerConfig for TcpConfig {
    type ClientPacket = CustomCryptClientPacket;
    type ServerPacket = CustomCryptServerPacket;
    type Protocol = TcpProtocol;

    type SerializerError = CustomSerializationError;

    type LengthSerializer = LittleEndian<u32>;

    fn build_serializer(
    ) -> SerializerAdapter<Self::ClientPacket, Self::ServerPacket, Self::SerializerError> {
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
    type SerializerError = CustomSerializationError;

    type LengthSerializer = LittleEndian<u32>;
    fn build_serializer(
    ) -> SerializerAdapter<Self::ServerPacket, Self::ClientPacket, Self::SerializerError> {
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
    let srv_addr = "127.0.0.1:3004";
    let mut app_server = App::new();
    app_server.add_plugins(ServerPlugin::<TcpConfig>::bind(srv_addr));

    let mut app_client = App::new();
    app_client.add_plugins(ClientPlugin::<TcpConfig>::connect(srv_addr));

    app_server.update(); // bind
    app_client.update(); // connect
    std::thread::sleep(std::time::Duration::from_secs(1));
    app_client.update(); // add connection resource
    app_server.update(); // handle connection

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
    )
}

#[derive(Resource, Default)]
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
    let server_addr = "127.0.0.1:3005";

    let mut app_server = App::new();
    app_server.add_plugins(ServerPlugin::<TcpConfig>::bind(server_addr));
    app_server.insert_resource(ReceivedPackets::<CustomCryptClientPacket>::default());
    app_server.insert_resource(ServerToClientPacketResource(
        server_to_client_packet.clone(),
    ));

    app_server.observe(server_new_connection_system);
    app_server.observe(server_packet_receive_system);

    let mut app_client = App::new();
    app_client.add_plugins(ClientPlugin::<TcpConfig>::connect(server_addr));
    app_client.insert_resource(ReceivedPackets::<CustomCryptServerPacket>::default());
    app_client.insert_resource(ClientToServerPacketResource(
        client_to_server_packet.clone(),
    ));

    app_client.observe(client_connection_establish_system);
    app_client.observe(client_packet_receive_system);

    app_server.update(); // bind
    app_client.update(); // connect
    std::thread::sleep(Duration::from_secs(1));
    app_client.update(); // add connection resource
    app_server.update(); // handle connection
    std::thread::sleep(Duration::from_secs(1));
    app_client.update(); // handle packet
    app_server.update(); // handle packet

    // Check if the server received the packet from the client
    let server_received_packets = app_server
        .world()
        .get_resource::<ReceivedPackets<CustomCryptClientPacket>>()
        .unwrap();
    assert_eq!(
        server_received_packets.packets.get(0),
        Some(&client_to_server_packet),
        "Server did not receive the expected packet from client"
    );

    // Check if the client received the packet from the server
    let client_received_packets = app_client
        .world()
        .get_resource::<ReceivedPackets<CustomCryptServerPacket>>()
        .unwrap();
    assert_eq!(
        client_received_packets.packets.get(0),
        Some(&server_to_client_packet),
        "Client did not receive the expected packet from server"
    );
}

fn server_new_connection_system(
    event: Trigger<NewConnectionEvent<TcpConfig>>,
    server_to_client_packet: Res<ServerToClientPacketResource>,
) {
    event
        .event()
        .connection
        .send(server_to_client_packet.0.clone())
        .expect("Couldn't send server packet");
}

fn server_packet_receive_system(
    event: Trigger<server::PacketReceiveEvent<TcpConfig>>,
    mut received_packets: ResMut<ReceivedPackets<CustomCryptClientPacket>>,
) {
    received_packets.packets.push(event.event().packet.clone());
}

fn client_connection_establish_system(
    event: Trigger<ConnectionEstablishEvent<TcpConfig>>,
    client_to_server_packet: Res<ClientToServerPacketResource>,
) {
    event
        .event()
        .connection
        .send(client_to_server_packet.0.clone())
        .expect("Couldn't send client packet");
}

fn client_packet_receive_system(
    event: Trigger<client::PacketReceiveEvent<TcpConfig>>,
    mut received_packets: ResMut<ReceivedPackets<CustomCryptServerPacket>>,
) {
    received_packets.packets.push(event.event().packet.clone());
}
