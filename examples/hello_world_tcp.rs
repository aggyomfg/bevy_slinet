use std::sync::Arc;
use std::time::Duration;

use bevy::prelude::*;
use bevy_slinet::serializer::SerializerAdapter;
use bincode::DefaultOptions;
use serde::{Deserialize, Serialize};

use bevy_slinet::client::ClientPlugin;
use bevy_slinet::packet_length_serializer::LittleEndian;
use bevy_slinet::protocols::tcp::TcpProtocol;
use bevy_slinet::serializers::bincode::BincodeSerializer;
use bevy_slinet::server::{NewConnectionEvent, ServerPlugin};
use bevy_slinet::{client, server, ClientConfig, ServerConfig};

struct Config;

impl ServerConfig for Config {
    type ClientPacket = ClientPacket;
    type ServerPacket = ServerPacket;
    type Protocol = TcpProtocol;
    type SerializerError = bincode::Error;
    fn build_serializer(
    ) -> SerializerAdapter<Self::ClientPacket, Self::ServerPacket, Self::SerializerError> {
        SerializerAdapter::ReadOnly(Arc::new(BincodeSerializer::<DefaultOptions>::default()))
    }
    type LengthSerializer = LittleEndian<u32>;
}

impl ClientConfig for Config {
    type ClientPacket = ClientPacket;
    type ServerPacket = ServerPacket;
    type Protocol = TcpProtocol;
    type SerializerError = bincode::Error;
    fn build_serializer(
    ) -> SerializerAdapter<Self::ServerPacket, Self::ClientPacket, Self::SerializerError> {
        SerializerAdapter::ReadOnly(Arc::new(BincodeSerializer::<DefaultOptions>::default()))
    }
    type LengthSerializer = LittleEndian<u32>;
}

#[derive(Serialize, Deserialize, Debug)]
enum ClientPacket {
    String(String),
}

#[derive(Serialize, Deserialize, Debug)]
enum ServerPacket {
    String(String),
}

fn main() {
    let server_addr = "127.0.0.1:3000";
    let server = std::thread::spawn(move || {
        App::new()
            .add_plugins((MinimalPlugins, ServerPlugin::<Config>::bind(server_addr)))
            .observe(server_new_connection_system)
            .observe(server_packet_receive_system)
            .run();
    });
    println!("Waiting 1000ms to make sure the server side has started");
    std::thread::sleep(Duration::from_millis(1000));
    let client = std::thread::spawn(move || {
        App::new()
            .add_plugins(MinimalPlugins)
            .add_plugins(ClientPlugin::<Config>::connect(server_addr))
            .observe(client_packet_receive_system)
            .run();
    });
    server.join().unwrap();
    client.join().unwrap();
}

fn server_new_connection_system(new_connection: Trigger<NewConnectionEvent<Config>>) {
    new_connection
        .event()
        .connection
        .send(ServerPacket::String("Hello, World!".to_string()))
        .unwrap();
    println!(
        "New connection from {:?}",
        new_connection.event().connection.peer_addr()
    );
}

fn client_packet_receive_system(new_packet: Trigger<client::PacketReceiveEvent<Config>>) {
    match &new_packet.event().packet {
        ServerPacket::String(s) => println!("Server -> Client: {s}"),
    }
    new_packet
        .event()
        .connection
        .send(ClientPacket::String("Hello, Server!".to_string()))
        .unwrap();
}

fn server_packet_receive_system(new_packet: Trigger<server::PacketReceiveEvent<Config>>) {
    match &new_packet.event().packet {
        ClientPacket::String(s) => println!("Server <- Client: {s}"),
    }
    new_packet
        .event()
        .connection
        .send(ServerPacket::String("Hello, Client!".to_string()))
        .unwrap();
}
