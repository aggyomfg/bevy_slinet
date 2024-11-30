use std::sync::Arc;
use std::time::Duration;

use bevy::prelude::*;
use bevy_slinet::serializer::SerializerAdapter;
use bincode::DefaultOptions;
use serde::{Deserialize, Serialize};

use bevy_slinet::client::ClientPlugin;
use bevy_slinet::packet_length_serializer::LittleEndian;
use bevy_slinet::protocols::udp::UdpProtocol;
use bevy_slinet::serializers::bincode::BincodeSerializer;
use bevy_slinet::server::{NewConnectionEvent, ServerPlugin};
use bevy_slinet::{client, server, ClientConfig, ServerConfig};

struct Config;

impl ServerConfig for Config {
    type ClientPacket = ClientPacket;
    type ServerPacket = ServerPacket;
    type Protocol = UdpProtocol;
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
    type Protocol = UdpProtocol;
    type SerializerError = bincode::Error;
    fn build_serializer(
    ) -> SerializerAdapter<Self::ServerPacket, Self::ClientPacket, Self::SerializerError> {
        SerializerAdapter::ReadOnly(Arc::new(BincodeSerializer::<DefaultOptions>::default()))
    }
    type LengthSerializer = LittleEndian<u32>;
}
#[derive(Serialize, Deserialize, Debug)]
enum ServerPacket {
    Hello,
    Message(usize),
}

#[derive(Serialize, Deserialize, Debug)]
enum ClientPacket {
    Hello,
    Reply(usize),
}

#[derive(Resource)]
struct ClientId(usize);

fn main() {
    let server = std::thread::spawn(move || {
        App::new()
            .add_plugins((
                MinimalPlugins,
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
    server.join().unwrap();
}

fn server_new_connection_system(new_connection: Trigger<NewConnectionEvent<Config>>) {
    new_connection
        .event()
        .connection
        .send(ServerPacket::Hello)
        .unwrap();
    println!(
        "Connection from {:?}",
        new_connection.event().connection.peer_addr()
    );
}

fn client_packet_receive_system(
    new_packet: Trigger<client::PacketReceiveEvent<Config>>,
    client_number: Res<ClientId>,
) {
    match &new_packet.event().packet {
        ServerPacket::Hello => {
            println!("Server -> Client: Hello (client #{})", client_number.0);
            new_packet
                .event()
                .connection
                .send(ClientPacket::Hello)
                .unwrap();
        }
        ServerPacket::Message(i) => {
            println!("Server -> Client: {} (client #{})", i, client_number.0);
            new_packet
                .event()
                .connection
                .send(ClientPacket::Reply(*i))
                .unwrap();
        }
    }
}

fn server_packet_receive_system(new_packet: Trigger<server::PacketReceiveEvent<Config>>) {
    match &new_packet.event().packet {
        ClientPacket::Hello => {
            println!(
                "Server <- Client {:04?}: Hello",
                new_packet.event().connection.id()
            );
            new_packet
                .event()
                .connection
                .send(ServerPacket::Message(0))
                .unwrap();
        }
        ClientPacket::Reply(i) => {
            println!(
                "Server <- Client {:04?}: {}",
                new_packet.event().connection.id(),
                i
            );
            new_packet
                .event()
                .connection
                .send(ServerPacket::Message(i + 1))
                .unwrap();
        }
    }
}
