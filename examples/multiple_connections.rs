use std::convert::Infallible;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use bevy::ecs::error::{ResultSeverityExt, Severity};
use bevy::log::LogPlugin;
use bevy::prelude::*;
use bevy_slinet::serializer::SerializerAdapter;

use bevy_slinet::client::ClientPlugin;
use bevy_slinet::connection::ConnectionId;
use bevy_slinet::packet_length_serializer::LittleEndian;
use bevy_slinet::protocols::udp::UdpProtocol;
use bevy_slinet::serializers::bitcode::BitcodeSerializer;
use bevy_slinet::server::{NewConnectionEvent, ServerPlugin};
use bevy_slinet::{client, server, ClientConfig, ServerConfig};
use bitcode::{Decode, Encode};

struct Config;

impl ServerConfig for Config {
    type ClientPacket = ClientPacket;
    type ServerPacket = ServerPacket;
    type Protocol = UdpProtocol;
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
    type LengthSerializer = LittleEndian<u32>;
}

impl ClientConfig for Config {
    type ClientPacket = ClientPacket;
    type ServerPacket = ServerPacket;
    type Protocol = UdpProtocol;
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
    type LengthSerializer = LittleEndian<u32>;
}
#[derive(Debug, Decode, Encode)]
enum ServerPacket {
    Hello,
    Message(SequenceNumber),
}

#[derive(Debug, Decode, Encode)]
enum ClientPacket {
    Hello,
    Reply(SequenceNumber),
}

#[derive(Clone, Copy, Debug, Decode, Encode)]
struct SequenceNumber(usize);

impl SequenceNumber {
    const FIRST: Self = Self(0);

    const fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

impl fmt::Display for SequenceNumber {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Resource)]
struct ClientId(usize);

impl fmt::Display for ClientId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl ServerPacket {
    fn reply(&self, client: &ClientId) -> ClientPacket {
        match self {
            Self::Hello => {
                println!("Server -> Client: Hello (client #{client})");
                ClientPacket::Hello
            }
            Self::Message(sequence) => {
                println!("Server -> Client: {sequence} (client #{client})");
                ClientPacket::Reply(*sequence)
            }
        }
    }
}

impl ClientPacket {
    fn reply(&self, connection: ConnectionId) -> ServerPacket {
        match self {
            Self::Hello => {
                println!("Server <- Client {connection:04?}: Hello");
                ServerPacket::Message(SequenceNumber::FIRST)
            }
            Self::Reply(sequence) => {
                println!("Server <- Client {connection:04?}: {sequence}");
                ServerPacket::Message(sequence.next())
            }
        }
    }
}

fn main() -> std::thread::Result<()> {
    let server = std::thread::spawn(move || {
        App::new()
            .add_plugins((
                MinimalPlugins,
                LogPlugin::default(),
                ServerPlugin::<Config>::bind("127.0.0.1:3000"),
            ))
            .add_observer(server_new_connection_system)
            .add_observer(server_packet_receive_system)
            .run();
    });
    println!("Waiting 1000ms to make sure the server has started");
    std::thread::sleep(Duration::from_millis(1000));
    for id in 0..10 {
        std::thread::spawn(move || {
            App::new()
                .add_plugins((
                    MinimalPlugins,
                    ClientPlugin::<Config>::connect("127.0.0.1:3000"),
                ))
                .insert_resource(ClientId(id))
                .add_observer(client_packet_receive_system)
                .run();
        });
    }
    server.join()?;
    Ok(())
}

fn server_new_connection_system(new_connection: On<NewConnectionEvent<Config>>) -> Result {
    let connection = &new_connection.event().connection;
    connection
        .send(ServerPacket::Hello)
        .with_severity(Severity::Error)?;
    println!("Connection from {:?}", connection.peer_addr());
    Ok(())
}

fn client_packet_receive_system(
    new_packet: On<client::PacketReceiveEvent<Config>>,
    client_id: Res<ClientId>,
) -> Result {
    let event = new_packet.event();
    event
        .connection
        .send(event.packet.reply(&client_id))
        .with_severity(Severity::Error)
}

fn server_packet_receive_system(new_packet: On<server::PacketReceiveEvent<Config>>) -> Result {
    let event = new_packet.event();
    event
        .connection
        .send(event.packet.reply(event.connection.id()))
        .with_severity(Severity::Error)
}
