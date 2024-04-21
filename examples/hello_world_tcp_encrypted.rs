use std::sync::{Arc, Mutex};
use std::time::Duration;

use bevy::prelude::*;
use bevy_slinet::serializer::SerializerAdapter;
use bevy_slinet::serializers::custom_crypt::{
    CustomCryptClientPacket, CustomCryptEngine, CustomCryptSerializer, CustomCryptServerPacket, CustomSerializationError,
};

use serde::{Deserialize, Serialize};

use bevy_slinet::client::ClientPlugin;
use bevy_slinet::packet_length_serializer::LittleEndian;
use bevy_slinet::protocols::tcp::TcpProtocol;

use bevy_slinet::server::{NewConnectionEvent, ServerPlugin};
use bevy_slinet::{client, server, ClientConfig, ServerConfig};

struct Config;

impl ServerConfig for Config {
    type ClientPacket = CustomCryptClientPacket;
    type ServerPacket = CustomCryptServerPacket;
    type Protocol = TcpProtocol;
    type SerializerError = CustomSerializationError;
    fn build_serializer(
    ) -> SerializerAdapter<Self::ClientPacket, Self::ServerPacket, Self::SerializerError> {
        SerializerAdapter::Mutable(Arc::new(Mutex::new(CustomCryptSerializer::<
            CustomCryptEngine,
            CustomCryptClientPacket,
            CustomCryptServerPacket,
        >::new(
            CustomCryptEngine::default()
        ))))
    }
    type LengthSerializer = LittleEndian<u32>;
}

impl ClientConfig for Config {
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
            .add_systems(
                Update,
                (server_new_connection_system, server_packet_receive_system),
            )
            .run();
    });
    println!("Waiting 5000ms to make sure the server side has started");
    std::thread::sleep(Duration::from_millis(5000));
    let client = std::thread::spawn(move || {
        App::new()
            .add_plugins(MinimalPlugins)
            .add_plugins(ClientPlugin::<Config>::connect(server_addr))
            .add_systems(Update, client_packet_receive_system)
            .run();
    });
    let client2 = std::thread::spawn(move || {
        App::new()
            .add_plugins(MinimalPlugins)
            .add_plugins(ClientPlugin::<Config>::connect(server_addr))
            .add_systems(Update, client2_packet_receive_system)
            .run();
    });
    server.join().unwrap();
    client.join().unwrap();
    client2.join().unwrap();
}

fn server_new_connection_system(mut events: EventReader<NewConnectionEvent<Config>>) {
    for event in events.read() {
        event
            .connection
            .send(CustomCryptServerPacket::String("Hello, World!".to_string()))
            .unwrap();
    }
}

fn client_packet_receive_system(mut events: EventReader<client::PacketReceiveEvent<Config>>) {
    for event in events.read() {
        match &event.packet {
            CustomCryptServerPacket::String(s) => println!("Server -> Client: {s}"),
        }
        event
            .connection
            .send(CustomCryptClientPacket::String(
                "Hello, Server!".to_string(),
            ))
            .unwrap();
    }
}

fn client2_packet_receive_system(mut events: EventReader<client::PacketReceiveEvent<Config>>) {
    for event in events.read() {
        match &event.packet {
            CustomCryptServerPacket::String(s) => println!("Server -> Client2: {s}"),
        }
        event
            .connection
            .send(CustomCryptClientPacket::String(
                "Hello, Server!, I'm Client2".to_string(),
            ))
            .unwrap();
    }
}

fn server_packet_receive_system(mut events: EventReader<server::PacketReceiveEvent<Config>>) {
    for event in events.read() {
        match &event.packet {
            CustomCryptClientPacket::String(s) => println!("Server <- Client: {s}"),
        }
        event
            .connection
            .send(CustomCryptServerPacket::String(
                "Hello, Client!".to_string(),
            ))
            .unwrap();
    }
}
