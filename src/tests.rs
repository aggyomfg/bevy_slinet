use crate::client;
use crate::client::{ClientConnection, ClientPlugin, ConnectionEstablishEvent};
use crate::packet_length_serializer::LittleEndian;
use crate::protocols::tcp::TcpProtocol;
use crate::serializer::SerializerAdapter;
use crate::serializers::bincode::BincodeSerializer;
use crate::server::{NewConnectionEvent, ServerConnections, ServerPlugin};
use crate::{server, ClientConfig, ServerConfig};
use bevy::app::App;
use bevy::prelude::*;
use bincode::DefaultOptions;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

#[derive(Default, Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
struct Packet(u64);

struct TcpConfig;

impl ServerConfig for TcpConfig {
    type ClientPacket = Packet;
    type ServerPacket = Packet;
    type Protocol = TcpProtocol;

    type SerializerError = bincode::Error;

    type LengthSerializer = LittleEndian<u32>;

    fn build_serializer(
    ) -> SerializerAdapter<Self::ClientPacket, Self::ServerPacket, Self::SerializerError> {
        SerializerAdapter::ReadOnly(Arc::new(BincodeSerializer::<DefaultOptions>::default()))
    }
}

impl ClientConfig for TcpConfig {
    type ClientPacket = Packet;
    type ServerPacket = Packet;
    type Protocol = TcpProtocol;
    type SerializerError = bincode::Error;

    type LengthSerializer = LittleEndian<u32>;
    fn build_serializer(
    ) -> SerializerAdapter<Self::ClientPacket, Self::ServerPacket, Self::SerializerError> {
        SerializerAdapter::ReadOnly(Arc::new(BincodeSerializer::<DefaultOptions>::default()))
    }
}

#[test]
fn tcp_connection() {
    let server_addr = "127.0.0.1:3000";

    let mut app_server = App::new();
    app_server.add_plugins(ServerPlugin::<TcpConfig>::bind(server_addr));

    let mut app_client = App::new();
    app_client.add_plugins(ClientPlugin::<TcpConfig>::connect(server_addr));

    app_server.update(); // bind
    app_client.update(); // connect
    std::thread::sleep(Duration::from_secs(1));
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
    );
}

#[derive(Resource, Default)]
struct ReceivedPackets<T> {
    packets: Vec<T>,
}

#[derive(Resource)]
struct ClientToServerPacketResource(Packet);

#[derive(Resource)]
struct ServerToClientPacketResource(Packet);

#[test]
fn tcp_packets() {
    let client_to_server_packet = Packet(42);
    let server_to_client_packet = Packet(24);
    let server_addr = "127.0.0.1:3007";

    let mut app_server = App::new();
    app_server.add_plugins(ServerPlugin::<TcpConfig>::bind(server_addr));
    app_server.insert_resource(ReceivedPackets::<Packet>::default());
    app_server.insert_resource(ServerToClientPacketResource(
        server_to_client_packet.clone(),
    ));

    app_server.observe(server_new_connection_system);
    app_server.observe(server_packet_receive_system);

    let mut app_client = App::new();
    app_client.add_plugins(ClientPlugin::<TcpConfig>::connect(server_addr));
    app_client.insert_resource(ReceivedPackets::<Packet>::default());
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
        .get_resource::<ReceivedPackets<Packet>>()
        .unwrap();
    assert_eq!(
        server_received_packets.packets.get(0),
        Some(&client_to_server_packet),
        "Server did not receive the expected packet from client"
    );

    // Check if the client received the packet from the server
    let client_received_packets = app_client
        .world()
        .get_resource::<ReceivedPackets<Packet>>()
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
    mut received_packets: ResMut<ReceivedPackets<Packet>>,
) {
    received_packets.packets.push(event.event().packet);
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
    mut received_packets: ResMut<ReceivedPackets<Packet>>,
) {
    received_packets.packets.push(event.event().packet);
}
