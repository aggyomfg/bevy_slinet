use std::time::Duration;

use bevy::prelude::*;
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
    type Serializer = BincodeSerializer<DefaultOptions>;
    type LengthSerializer = LittleEndian<u32>;
}

impl ClientConfig for Config {
    type ClientPacket = ClientPacket;
    type ServerPacket = ServerPacket;
    type Protocol = UdpProtocol;
    type Serializer = BincodeSerializer<DefaultOptions>;
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

struct ClientId(usize);

fn main() {
    let server = std::thread::spawn(move || {
        App::new()
            .add_plugins(MinimalPlugins)
            .add_plugin(ServerPlugin::<Config>::bind("127.0.0.1:3000").unwrap())
            .add_system(server_new_connection_system)
            .add_system(server_packet_receive_system)
            .run();
    });
    println!("Waiting 5000ms to make sure the server has started");
    std::thread::sleep(Duration::from_millis(5000));
    for id in 0..10 {
        std::thread::spawn(move || {
            App::new()
                .add_plugins(MinimalPlugins)
                .add_plugin(ClientPlugin::<Config>::connect("127.0.0.1:3000"))
                .insert_resource(ClientId(id))
                .add_system(client_packet_receive_system)
                .run();
        });
    }
    server.join().unwrap();
}

fn server_new_connection_system(mut events: EventReader<NewConnectionEvent<Config>>) {
    for event in events.iter() {
        println!("Connection");
        event.connection.send(ServerPacket::Hello).unwrap();
    }
}

fn client_packet_receive_system(
    mut events: EventReader<client::PacketReceiveEvent<Config>>,
    client_number: Res<ClientId>,
) {
    for event in events.iter() {
        match &event.packet {
            ServerPacket::Hello => {
                println!("Server -> Client: Hello (client #{})", client_number.0);
                event.connection.send(ClientPacket::Hello).unwrap();
            }
            ServerPacket::Message(i) => {
                println!("Server -> Client: {} (client #{})", i, client_number.0);
                event.connection.send(ClientPacket::Reply(*i)).unwrap();
            }
        }
    }
}

fn server_packet_receive_system(mut events: EventReader<server::PacketReceiveEvent<Config>>) {
    for event in events.iter() {
        match &event.packet {
            ClientPacket::Hello => {
                println!("Server <- Client {:04?}: Hello", event.connection.id());
                event.connection.send(ServerPacket::Message(0)).unwrap();
            }
            ClientPacket::Reply(i) => {
                println!("Server <- Client {:04?}: {}", event.connection.id(), i);
                event.connection.send(ServerPacket::Message(i + 1)).unwrap();
            }
        }
    }
}
