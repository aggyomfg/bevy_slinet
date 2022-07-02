use crate::client::{ClientConnection, ClientPlugin, ConnectionEstablishEvent};
use crate::packet_length_serializer::LittleEndian;
use crate::protocols::tcp::TcpProtocol;
use crate::serializers::bincode::BincodeSerializer;
use crate::server::{NewConnectionEvent, ServerConnection, ServerPlugin};
use crate::{client, server, ClientConfig, ServerConfig};
use bevy::app::App;
use bevy::ecs::event::Events;
use bevy::prelude::EventReader;
use bincode::DefaultOptions;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
struct Packet(u64);

struct TcpConfig;

impl ServerConfig for TcpConfig {
    type ClientPacket = Packet;
    type ServerPacket = Packet;
    type Protocol = TcpProtocol;
    type Serializer = BincodeSerializer<DefaultOptions>;
    type LengthSerializer = LittleEndian<u32>;
}

impl ClientConfig for TcpConfig {
    type ClientPacket = Packet;
    type ServerPacket = Packet;
    type Protocol = TcpProtocol;
    type Serializer = BincodeSerializer<DefaultOptions>;
    type LengthSerializer = LittleEndian<u32>;
}

#[test]
fn tcp_connection() {
    let mut app_server = App::new();
    app_server.add_plugin(ServerPlugin::<TcpConfig>::bind("127.0.0.1:3000"));

    let mut app_client = App::new();
    app_client.add_plugin(ClientPlugin::<TcpConfig>::connect("127.0.0.1:3000"));

    app_server.update(); // bind
    app_client.update(); // connect
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
            .resource::<Vec<ServerConnection<TcpConfig>>>()
            .len(),
        1,
        "Vec<ServerConnection>'s length is not 1"
    );
}

#[test]
fn tcp_packets() {
    let client_to_server_packet = Packet(42);
    let server_to_client_packet = Packet(24);

    let mut app_server = App::new();
    app_server.add_plugin(ServerPlugin::<TcpConfig>::bind("127.0.0.1:3001"));
    app_server.add_system(
        move |mut events: EventReader<NewConnectionEvent<TcpConfig>>| {
            for event in events.iter() {
                event
                    .connection
                    .send(server_to_client_packet)
                    .expect("Couldn't send server packet");
            }
        },
    );

    let mut app_client = App::new();
    app_client.add_plugin(ClientPlugin::<TcpConfig>::connect("127.0.0.1:3001"));
    app_client.add_system(
        move |mut events: EventReader<ConnectionEstablishEvent<TcpConfig>>| {
            for event in events.iter() {
                event
                    .connection
                    .send(client_to_server_packet)
                    .expect("Couldn't send client packet");
            }
        },
    );

    app_server.update(); // bind
    app_client.update(); // connect
    app_client.update(); // add connection resource
    app_server.update(); // handle connection
    app_client.update(); // handle packet
    app_server.update(); // handle packet

    let server_events = app_server
        .world
        .resource::<Events<server::PacketReceiveEvent<TcpConfig>>>();
    let mut server_reader = server_events.get_reader();
    let mut server_events_iter = server_reader.iter(server_events);
    assert_eq!(
        server_events_iter.next().map(|event| event.packet),
        Some(client_to_server_packet),
        "Serverside PacketReceiveEvent was not fired"
    );

    let client_events = app_client
        .world
        .resource::<Events<client::PacketReceiveEvent<TcpConfig>>>();
    let mut client_reader = client_events.get_reader();
    let mut client_events_iter = client_reader.iter(client_events);
    assert_eq!(
        client_events_iter.next().map(|event| event.packet),
        Some(server_to_client_packet),
        "Serverside PacketReceiveEvent was not fired"
    );
}
