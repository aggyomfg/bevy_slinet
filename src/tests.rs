use crate::client;
use crate::client::{ClientConnection, ClientPlugin, ConnectionEstablishEvent};
use crate::packet_length_serializer::LittleEndian;
use crate::protocols::tcp::TcpProtocol;
use crate::serializer::SerializerAdapter;
use crate::serializers::bincode_serde::BincodeSerdeSerializer;
use crate::server::{NewConnectionEvent, ServerAddress, ServerConnections, ServerPlugin};
use crate::{server, ClientConfig, ServerConfig};
use bevy::app::App;
use bevy::prelude::*;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Calls `step` until it returns `true` or a timeout expires.
pub(crate) fn wait_until(mut step: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !step() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
struct Packet(u64);

struct TcpConfig;

impl ServerConfig for TcpConfig {
    type ClientPacket = Packet;
    type ServerPacket = Packet;
    type Protocol = TcpProtocol;

    type EncodeError = bincode::error::EncodeError;
    type DecodeError = bincode::error::DecodeError;

    type LengthSerializer = LittleEndian<u32>;

    fn build_serializer() -> SerializerAdapter<
        Self::ClientPacket,
        Self::ServerPacket,
        Self::EncodeError,
        Self::DecodeError,
    > {
        SerializerAdapter::ReadOnly(Arc::new(BincodeSerdeSerializer::default()))
    }
}

impl ClientConfig for TcpConfig {
    type ClientPacket = Packet;
    type ServerPacket = Packet;
    type Protocol = TcpProtocol;
    type EncodeError = bincode::error::EncodeError;
    type DecodeError = bincode::error::DecodeError;

    type LengthSerializer = LittleEndian<u32>;
    fn build_serializer() -> SerializerAdapter<
        Self::ServerPacket,
        Self::ClientPacket,
        Self::EncodeError,
        Self::DecodeError,
    > {
        SerializerAdapter::ReadOnly(Arc::new(BincodeSerdeSerializer::default()))
    }
}

#[derive(Default, Resource)]
struct NewConnectionAddress(Option<SocketAddr>);

#[test]
fn tcp_connection() {
    let mut app_server = App::new();
    app_server.add_plugins(ServerPlugin::<TcpConfig>::bind("127.0.0.1:0"));
    app_server.init_resource::<NewConnectionAddress>();
    app_server.add_observer(
        |event: On<NewConnectionEvent<TcpConfig>>, mut address: ResMut<NewConnectionAddress>| {
            address.0 = Some(event.event().address);
        },
    );
    app_server.update(); // bind
    let server_addr = app_server
        .world()
        .resource::<ServerAddress<TcpConfig>>()
        .address();

    let mut app_client = App::new();
    app_client.add_plugins(ClientPlugin::<TcpConfig>::connect(server_addr));

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
    assert_eq!(
        app_server.world().resource::<NewConnectionAddress>().0,
        Some(
            app_client
                .world()
                .resource::<ClientConnection<TcpConfig>>()
                .local_addr()
        ),
        "NewConnectionEvent.address must be the client's address"
    );
}

#[derive(Default, Resource)]
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

    let mut app_server = App::new();
    app_server.add_plugins(ServerPlugin::<TcpConfig>::bind("127.0.0.1:0"));
    app_server.insert_resource(ReceivedPackets::<Packet>::default());
    app_server.insert_resource(ServerToClientPacketResource(server_to_client_packet));

    app_server.add_observer(server_new_connection_system);
    app_server.add_observer(server_packet_receive_system);

    app_server.update(); // bind
    let server_addr = app_server
        .world()
        .resource::<ServerAddress<TcpConfig>>()
        .address();

    let mut app_client = App::new();
    app_client.add_plugins(ClientPlugin::<TcpConfig>::connect(server_addr));
    app_client.insert_resource(ReceivedPackets::<Packet>::default());
    app_client.insert_resource(ClientToServerPacketResource(client_to_server_packet));

    app_client.add_observer(client_connection_establish_system);
    app_client.add_observer(client_packet_receive_system);

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
}

fn server_new_connection_system(
    event: On<NewConnectionEvent<TcpConfig>>,
    server_to_client_packet: Res<ServerToClientPacketResource>,
) {
    event
        .event()
        .connection
        .send(server_to_client_packet.0)
        .expect("Couldn't send server packet");
}

fn server_packet_receive_system(
    event: On<server::PacketReceiveEvent<TcpConfig>>,
    mut received_packets: ResMut<ReceivedPackets<Packet>>,
) {
    received_packets.packets.push(event.event().packet);
}

fn client_connection_establish_system(
    event: On<ConnectionEstablishEvent<TcpConfig>>,
    client_to_server_packet: Res<ClientToServerPacketResource>,
) {
    event
        .event()
        .connection
        .send(client_to_server_packet.0)
        .expect("Couldn't send client packet");
}

fn client_packet_receive_system(
    event: On<client::PacketReceiveEvent<TcpConfig>>,
    mut received_packets: ResMut<ReceivedPackets<Packet>>,
) {
    received_packets.packets.push(event.event().packet);
}
