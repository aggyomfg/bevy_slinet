use crate::client;
use crate::client::{ClientConnection, ClientPlugin, ConnectionEstablishEvent};
use crate::packet_length_serializer::LittleEndian;
use crate::protocols::tcp::TcpProtocol;
use crate::serializer::SerializerAdapter;
use crate::serializers::bincode::BincodeSerializer;
use crate::server::{NewConnectionEvent, ServerConnections, ServerPlugin};
use crate::{server, ClientConfig, ServerConfig};
use bevy::app::App;
use bevy::ecs::event::Events;
use bevy::prelude::{EventReader, Update};
use bincode::DefaultOptions;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
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
    let mut app_server = App::new();
    app_server.add_plugins(ServerPlugin::<TcpConfig>::bind("127.0.0.1:3000"));

    let mut app_client = App::new();
    app_client.add_plugins(ClientPlugin::<TcpConfig>::connect("127.0.0.1:3000"));

    app_server.update(); // bind
    app_client.update(); // connect
    std::thread::sleep(Duration::from_secs(1));
    app_client.update(); // add connection resource
    app_server.update(); // handle connection

    assert!(
        app_client
            .world
            .get_resource::<ClientConnection<TcpConfig>>()
            .is_some(),
        "No ClientConnection resource found"
    );
    assert_eq!(
        app_server
            .world
            .get_resource::<ServerConnections<TcpConfig>>()
            .unwrap()
            .len(),
        1,
    )
}

#[test]
fn tcp_packets() {
    let client_to_server_packet = Packet(42);
    let server_to_client_packet = Packet(24);
    let server_addr = "127.0.0.1:3007";

    let mut app_server = App::new();
    app_server.add_plugins(ServerPlugin::<TcpConfig>::bind(server_addr));
    app_server.add_systems(
        Update,
        move |mut events: EventReader<NewConnectionEvent<TcpConfig>>| {
            for event in events.read() {
                event
                    .connection
                    .send(server_to_client_packet)
                    .expect("Couldn't send server packet");
            }
        },
    );

    let mut app_client = App::new();
    app_client.add_plugins(ClientPlugin::<TcpConfig>::connect(server_addr));
    app_client.add_systems(
        Update,
        move |mut events: EventReader<ConnectionEstablishEvent<TcpConfig>>| {
            for event in events.read() {
                event
                    .connection
                    .send(client_to_server_packet)
                    .expect("Couldn't send client packet");
            }
        },
    );

    app_server.update(); // bind
    app_client.update(); // connect
    std::thread::sleep(Duration::from_secs(1));
    app_client.update(); // add connection resource
    app_server.update(); // handle connection
    std::thread::sleep(Duration::from_secs(1));
    app_client.update(); // handle packet
    app_server.update(); // handle packet

    let server_events = app_server
        .world
        .resource::<Events<server::PacketReceiveEvent<TcpConfig>>>();
    let mut server_reader = server_events.get_reader();
    let mut server_events_iter = server_reader.read(server_events);
    assert_eq!(
        server_events_iter.next().map(|event| event.packet),
        Some(client_to_server_packet),
        "Serverside PacketReceiveEvent was not fired"
    );

    let client_events = app_client
        .world
        .resource::<Events<client::PacketReceiveEvent<TcpConfig>>>();
    let mut client_reader = client_events.get_reader();
    let mut client_events_iter = client_reader.read(client_events);
    assert_eq!(
        client_events_iter.next().map(|event| event.packet),
        Some(server_to_client_packet),
        "Serverside PacketReceiveEvent was not fired"
    );
}
