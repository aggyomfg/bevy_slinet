use bevy::app::{App, Update};
use bevy::ecs::event::{EventReader, Events};

use crate::client::{self, ClientConnection, ClientPlugin, ConnectionEstablishEvent};
use crate::packet_length_serializer::LittleEndian;
use crate::protocols::tcp::TcpProtocol;
use crate::serializer::{DefaultSerializationError, SerializerAdapter};
use crate::serializers::custom_crypt::{
    CustomCryptClientPacket, CustomCryptEngine, CustomCryptSerializer, CustomCryptServerPacket,
};
use crate::server::{self, NewConnectionEvent, ServerConnections, ServerPlugin};
use crate::{ClientConfig, ServerConfig};

use std::sync::{Arc, Mutex};

struct TcpConfig;

impl ServerConfig for TcpConfig {
    type ClientPacket = CustomCryptClientPacket;
    type ServerPacket = CustomCryptServerPacket;
    type Protocol = TcpProtocol;

    type SerializerError = DefaultSerializationError;

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
    type SerializerError = DefaultSerializationError;

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
    // Define server and client configurations
    let srv_addr = "127.0.0.1:3005";

    // Setup server and client applications
    let mut app_server = setup_server_app(srv_addr);
    let mut app_client = setup_client_app(srv_addr, "Hello, Server!");

    // Simulate application lifecycle
    run_simulation(&mut app_server, &mut app_client);

    // Check events and packets
    check_server_received_packets(&app_server);
    check_client_received_packets(&app_client, "Hello, Client!");
}

fn server_receive_system(mut events: EventReader<NewConnectionEvent<TcpConfig>>) {
    let server_to_client_packet = CustomCryptServerPacket::String(String::from("Hello, Client!"));
    for event in events.read() {
        event
            .connection
            .send(server_to_client_packet.clone())
            .expect("Failed to send packet to client");
    }
}

fn setup_server_app(srv_addr: &str) -> App {
    let mut app = App::new();
    app.add_plugins(ServerPlugin::<TcpConfig>::bind(srv_addr));
    app.add_systems(Update, server_receive_system);
    app
}

fn setup_client_app(srv_addr: &str, message: &str) -> App {
    let packet = CustomCryptClientPacket::String(String::from(message));
    let mut app = App::new();
    app.add_plugins(ClientPlugin::<TcpConfig>::connect(srv_addr));
    app.add_systems(
        Update,
        move |mut events: EventReader<ConnectionEstablishEvent<TcpConfig>>| {
            for event in events.read() {
                event
                    .connection
                    .send(packet.clone())
                    .expect("Failed to send packet");
            }
        },
    );
    app
}

// Simulate the test scenario
fn run_simulation(app_server: &mut App, app_client: &mut App) {
    app_server.update();
    app_client.update();
    std::thread::sleep(std::time::Duration::from_secs(1));
    app_client.update();
    app_server.update();
    std::thread::sleep(std::time::Duration::from_secs(1));
    app_client.update();
    app_server.update();
}

fn check_server_received_packets(app_server: &App) {
    let server_events = app_server
        .world
        .resource::<Events<server::PacketReceiveEvent<TcpConfig>>>();
    let mut server_reader = server_events.get_reader();
    let mut server_events_iter = server_reader.read(server_events);

    assert_eq!(
        server_events_iter.next().map(|event| event.packet.clone()),
        Some(CustomCryptClientPacket::String(String::from(
            "Hello, Server!"
        ))),
        "Server did not receive packet from client 1"
    );
}

fn check_client_received_packets(app_client: &App, expected_message: &str) {
    let client_events = app_client
        .world
        .resource::<Events<client::PacketReceiveEvent<TcpConfig>>>();
    let mut client_reader = client_events.get_reader();
    let mut client_events_iter = client_reader.read(client_events);

    assert_eq!(
        client_events_iter.next().map(|event| event.packet.clone()),
        Some(CustomCryptServerPacket::String(String::from(
            expected_message
        ))),
        "Client did not receive packet from server"
    );
}
