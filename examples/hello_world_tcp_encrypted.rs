use std::sync::{Arc, Mutex};
use std::time::Duration;

use bevy::ecs::error::{ResultSeverityExt, Severity};
use bevy::log::LogPlugin;
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
    type EncodeError = CustomSerializationError;
    type DecodeError = CustomSerializationError;
    fn build_serializer() -> SerializerAdapter<
        Self::ClientPacket,
        Self::ServerPacket,
        Self::EncodeError,
        Self::DecodeError,
    > {
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
fn main() {
    let server_addr = "127.0.0.1:3000";
    let server = std::thread::spawn(move || {
        App::new()
            .add_plugins((
                MinimalPlugins,
                LogPlugin::default(),
                ServerPlugin::<Config>::bind(server_addr),
            ))
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

fn server_new_connection_system(new_connection: On<NewConnectionEvent<Config>>) -> Result {
    new_connection
        .event()
        .connection
        .send(CustomCryptServerPacket::String("Hello, World!".to_string()))
        .with_severity(Severity::Error)?;
    println!(
        "New connection from: {:?}",
        new_connection.event().connection.peer_addr()
    );
    Ok(())
}

fn client_packet_receive_system(new_packet: On<client::PacketReceiveEvent<Config>>) -> Result {
    match &new_packet.event().packet {
        CustomCryptServerPacket::String(s) => println!("Server -> Client: {s}"),
    }
    new_packet
        .event()
        .connection
        .send(CustomCryptClientPacket::String(
            "Hello, Server!".to_string(),
        ))
        .with_severity(Severity::Error)?;
    Ok(())
}

fn client2_packet_receive_system(new_packet: On<client::PacketReceiveEvent<Config>>) -> Result {
    match &new_packet.event().packet {
        CustomCryptServerPacket::String(s) => println!("Server -> Client2: {s}"),
    }
    new_packet
        .event()
        .connection
        .send(CustomCryptClientPacket::String(
            "Hello, Server!, I'm Client2".to_string(),
        ))
        .with_severity(Severity::Error)?;
    Ok(())
}

fn server_packet_receive_system(new_packet: On<server::PacketReceiveEvent<Config>>) -> Result {
    match &new_packet.event().packet {
        CustomCryptClientPacket::String(s) => println!("Server <- Client: {s}"),
    }
    new_packet
        .event()
        .connection
        .send(CustomCryptServerPacket::String(
            "Hello, Client!".to_string(),
        ))
        .with_severity(Severity::Error)?;
    Ok(())
}
