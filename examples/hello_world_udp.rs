use std::time::Duration;

use bevy::app::{App, EventReader};
use bevy::MinimalPlugins;
use bincode::DefaultOptions;
use serde::{Deserialize, Serialize};

use bevy_slinet::client::ClientPlugin;
use bevy_slinet::packet_length_serializer::BigEndian;
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
    type LengthSerializer = BigEndian<u8>;
}

impl ClientConfig for Config {
    type ClientPacket = ClientPacket;
    type ServerPacket = ServerPacket;
    type Protocol = UdpProtocol;
    type Serializer = BincodeSerializer<DefaultOptions>;
    type LengthSerializer = BigEndian<u8>;
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
    let server = std::thread::spawn(move || {
        App::new()
            .add_plugins(MinimalPlugins)
            .add_plugin(ServerPlugin::<Config>::bind("127.0.0.1:3000").unwrap())
            .add_system(server_new_connection_system)
            .add_system(server_packet_receive_system)
            .run();
    });
    println!("Waiting 5000ms to make sure the server side has started");
    std::thread::sleep(Duration::from_millis(5000));
    let client = std::thread::spawn(move || {
        App::new()
            .add_plugins(MinimalPlugins)
            .add_plugin(ClientPlugin::<Config>::connect("127.0.0.1:3000"))
            .add_system(client_packet_receive_system)
            .run();
    });
    server.join().unwrap();
    client.join().unwrap();
}

fn server_new_connection_system(mut events: EventReader<NewConnectionEvent<Config>>) {
    for event in events.iter() {
        event
            .connection
            .send(ServerPacket::String("Hello, World!".to_string()))
            .unwrap();
    }
}

fn client_packet_receive_system(mut events: EventReader<client::PacketReceiveEvent<Config>>) {
    for event in events.iter() {
        match &event.packet {
            ServerPacket::String(s) => println!("Server -> Client: {}", s),
        }
        event
            .connection
            .send(ClientPacket::String("Hello, Server!".to_string()))
            .unwrap();
    }
}

fn server_packet_receive_system(mut events: EventReader<server::PacketReceiveEvent<Config>>) {
    for event in events.iter() {
        match &event.packet {
            ClientPacket::String(s) => println!("Server <- Client: {}", s),
        }
        event
            .connection
            .send(ServerPacket::String("Hello, Client!".to_string()))
            .unwrap();
    }
}
