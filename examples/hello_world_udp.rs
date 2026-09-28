use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use bevy::prelude::*;
use bevy_slinet::serializer::SerializerAdapter;

use bevy_slinet::client::ClientPlugin;
use bevy_slinet::connection::NetworkQueueSettings;
use bevy_slinet::packet_length_serializer::BigEndian;
use bevy_slinet::protocols::udp::{ConfiguredUdpProtocol, UdpConfig, UdpOptions};
use bevy_slinet::serializers::bitcode::BitcodeSerializer;
use bevy_slinet::server::{NewConnectionEvent, ServerPlugin};
use bevy_slinet::{client, server, ClientConfig, ServerConfig};
use bitcode::{Decode, Encode};

struct Config;

impl UdpConfig for Config {
    const OPTIONS: UdpOptions = UdpOptions {
        max_peers: 64,
        max_datagram_size: 1200, // Includes the session header; keep below the path MTU.
        heartbeat_interval: Duration::from_secs(1),
        heartbeat_jitter: Duration::from_millis(100),
        ..UdpOptions::DEFAULT
    };
}

impl ServerConfig for Config {
    type ClientPacket = ClientPacket;
    type ServerPacket = ServerPacket;
    type Protocol = ConfiguredUdpProtocol<Self>;
    type EncodeError = Infallible;
    type DecodeError = bitcode::Error;
    fn build_serializer() -> SerializerAdapter<
        Self::ClientPacket,
        Self::ServerPacket,
        Self::EncodeError,
        Self::DecodeError,
    > {
        SerializerAdapter::ReadOnly(Arc::new(BitcodeSerializer))
    }
    type LengthSerializer = BigEndian<u8>;
}

impl ClientConfig for Config {
    type ClientPacket = ClientPacket;
    type ServerPacket = ServerPacket;
    type Protocol = ConfiguredUdpProtocol<Self>;
    type EncodeError = Infallible;
    type DecodeError = bitcode::Error;
    fn build_serializer() -> SerializerAdapter<
        Self::ServerPacket,
        Self::ClientPacket,
        Self::EncodeError,
        Self::DecodeError,
    > {
        SerializerAdapter::ReadOnly(Arc::new(BitcodeSerializer))
    }
    type LengthSerializer = BigEndian<u8>;
}

#[derive(Debug, Decode, Encode)]
enum ClientPacket {
    String(String),
}

#[derive(Debug, Decode, Encode)]
enum ServerPacket {
    String(String),
}

fn main() {
    let server_addr = "127.0.0.1:3000";
    let server = std::thread::spawn(move || {
        App::new()
            .insert_resource(NetworkQueueSettings::default())
            .add_plugins((MinimalPlugins, ServerPlugin::<Config>::bind(server_addr)))
            .add_observer(server_new_connection_system)
            .add_observer(server_packet_receive_system)
            .run();
    });
    println!("Waiting 1000ms to make sure the server side has started");
    std::thread::sleep(Duration::from_millis(1000));
    let client = std::thread::spawn(move || {
        App::new()
            .insert_resource(NetworkQueueSettings::default())
            .add_plugins(MinimalPlugins)
            .add_plugins(ClientPlugin::<Config>::connect(server_addr))
            .add_observer(client_packet_receive_system)
            .run();
    });
    server.join().unwrap();
    client.join().unwrap();
}

fn server_new_connection_system(new_connection: On<NewConnectionEvent<Config>>) {
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

fn client_packet_receive_system(new_packet: On<client::PacketReceiveEvent<Config>>) {
    match &new_packet.event().packet {
        ServerPacket::String(s) => println!("Server -> Client: {s}"),
    }
    new_packet
        .event()
        .connection
        .send(ClientPacket::String("Hello, Server!".to_string()))
        .unwrap();
}

fn server_packet_receive_system(new_packet: On<server::PacketReceiveEvent<Config>>) {
    match &new_packet.event().packet {
        ClientPacket::String(s) => println!("Server <- Client: {s}"),
    }
    new_packet
        .event()
        .connection
        .send(ServerPacket::String("Hello, Client!".to_string()))
        .unwrap();
}
