use std::sync::{Arc, Mutex};
use std::time::Duration;

use bevy::prelude::*;
use bevy_slinet::serializer::SerializerAdapter;
use bevy_slinet::serializers::custom_crypt::{
    CustomCryptClientPacket, CustomCryptEngine, CustomCryptSerializer, CustomCryptServerPacket,
    CustomSerializationError,
};

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
fn main() {
    let server_addr = "127.0.0.1:3000";
    let server = std::thread::spawn(move || {
        App::new()
            .add_plugins((MinimalPlugins, ServerPlugin::<Config>::bind(server_addr)))
            .add_observer(server_new_connection_system)
            .add_observer(server_packet_receive_system)
            .run();
    });
    println!("Waiting 1000ms to make sure the server side has started");
    std::thread::sleep(Duration::from_millis(1000));
    let client = std::thread::spawn(move || {
        App::new()
            .add_plugins(MinimalPlugins)
            .add_plugins(ClientPlugin::<Config>::connect(server_addr))
            .add_observer(client_packet_receive_system)
            .run();
    });
    let client2 = std::thread::spawn(move || {
        App::new()
            .add_plugins(MinimalPlugins)
            .add_plugins(ClientPlugin::<Config>::connect(server_addr))
            .add_observer(client2_packet_receive_system)
            .run();
    });
    server.join().unwrap();
    client.join().unwrap();
    client2.join().unwrap();
}

fn server_new_connection_system(new_connection: Trigger<NewConnectionEvent<Config>>) {
    new_connection
        .event()
        .connection
        .send(CustomCryptServerPacket::String("Hello, World!".to_string()))
        .unwrap();
    println!(
        "New connection from: {:?}",
        new_connection.event().connection.peer_addr()
    );
}

fn client_packet_receive_system(new_packet: Trigger<client::PacketReceiveEvent<Config>>) {
    match &new_packet.event().packet {
        CustomCryptServerPacket::String(s) => println!("Server -> Client: {s}"),
    }
    new_packet
        .event()
        .connection
        .send(CustomCryptClientPacket::String(
            "Hello, Server!".to_string(),
        ))
        .unwrap();
}

fn client2_packet_receive_system(new_packet: Trigger<client::PacketReceiveEvent<Config>>) {
    match &new_packet.event().packet {
        CustomCryptServerPacket::String(s) => println!("Server -> Client2: {s}"),
    }
    new_packet
        .event()
        .connection
        .send(CustomCryptClientPacket::String(
            "Hello, Server!, I'm Client2".to_string(),
        ))
        .unwrap();
}

fn server_packet_receive_system(new_packet: Trigger<server::PacketReceiveEvent<Config>>) {
    match &new_packet.event().packet {
        CustomCryptClientPacket::String(s) => println!("Server <- Client: {s}"),
    }
    new_packet
        .event()
        .connection
        .send(CustomCryptServerPacket::String(
            "Hello, Client!".to_string(),
        ))
        .unwrap();
}
