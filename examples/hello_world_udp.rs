use std::convert::Infallible;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::Duration;

use bevy::ecs::error::{ResultSeverityExt, Severity};
use bevy::log::LogPlugin;
use bevy::prelude::*;
use bevy_slinet::serializers::serializer::SerializerAdapter;

use bevy_slinet::client::ClientPlugin;
use bevy_slinet::connection::{MaxPacketSize, NetworkQueueSettings, OverflowPolicy};
use bevy_slinet::protocols::udp::{ConfiguredUdpProtocol, UdpConfig, UdpOptions};
use bevy_slinet::serializers::bitcode::BitcodeSerializer;
use bevy_slinet::serializers::packet_length_serializer::BigEndian;
use bevy_slinet::server::{NewConnectionEvent, ServerPlugin};
use bevy_slinet::{client, server, ClientConfig, ServerConfig};
use bitcode::{Decode, Encode};

struct Config;

impl UdpConfig for Config {
    const OPTIONS: UdpOptions = UdpOptions {
        max_peers: 64,
        receive_queue_overflow: OverflowPolicy::DropOldest,
        send_rate: NonZeroU64::new(16 * 1024), // A rate cap; applications still control congestion.
        max_datagram_size: 1200,               // Full serialized payload; keep below the path MTU.
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

fn main() -> std::thread::Result<()> {
    let server_addr = "127.0.0.1:3000";
    let server = std::thread::spawn(move || {
        App::new()
            .insert_resource(MaxPacketSize(1200))
            .insert_resource(NetworkQueueSettings {
                datagram_receive_overflow: OverflowPolicy::DropOldest,
                ..NetworkQueueSettings::default()
            })
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
    std::thread::sleep(Duration::from_secs(1));
    let client = std::thread::spawn(move || {
        App::new()
            .insert_resource(MaxPacketSize(1200))
            .insert_resource(NetworkQueueSettings {
                datagram_receive_overflow: OverflowPolicy::DropOldest,
                ..NetworkQueueSettings::default()
            })
            .add_plugins(MinimalPlugins)
            .add_plugins(ClientPlugin::<Config>::connect(server_addr))
            .add_observer(
                |event: On<client::ConnectionEstablishEvent<Config>>| -> Result {
                    event
                        .connection
                        .send(ClientPacket::String("Hello, Server!".into()))
                        .with_severity(Severity::Error)?;
                    Ok(())
                },
            )
            .add_observer(client_packet_receive_system)
            .run();
    });
    server.join()?;
    client.join()?;
    Ok(())
}

fn server_new_connection_system(new_connection: On<NewConnectionEvent<Config>>) -> Result {
    let udp = new_connection.event().connection.transport();
    println!("UDP payload budget: {} bytes", udp.max_payload_size());
    // Clone the handle to retain its statistics after the connection is removed.
    udp.set_send_rate(NonZeroU64::new(16 * 1024));
    println!("Local UDP statistics: {:?}", udp.stats());
    new_connection
        .event()
        .connection
        .send(ServerPacket::String("Hello, World!".to_string()))
        .with_severity(Severity::Error)?;
    println!(
        "New connection from {:?}",
        new_connection.event().connection.peer_addr()
    );
    Ok(())
}

fn client_packet_receive_system(new_packet: On<client::PacketReceiveEvent<Config>>) -> Result {
    match &new_packet.event().packet {
        ServerPacket::String(s) => println!("Server -> Client: {s}"),
    }
    new_packet
        .event()
        .connection
        .send(ClientPacket::String("Hello, Server!".to_string()))
        .with_severity(Severity::Error)?;
    Ok(())
}

fn server_packet_receive_system(new_packet: On<server::PacketReceiveEvent<Config>>) -> Result {
    match &new_packet.event().packet {
        ClientPacket::String(s) => println!("Server <- Client: {s}"),
    }
    new_packet
        .event()
        .connection
        .send(ServerPacket::String("Hello, Client!".to_string()))
        .with_severity(Severity::Error)?;
    Ok(())
}
