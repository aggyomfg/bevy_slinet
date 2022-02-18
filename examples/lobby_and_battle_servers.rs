//! In this example we'll just .unwrap() results, but you should do something else.
//! How the example works (Lobby is TCP, Battle is UDP):
//!
//! Client -> Lobby: Hello
//!
//! Lobby -> Client: Hello
//!
//! Client -> Lobby: Battle
//!
//! Lobby -> Client: BattleServer(address of battle server)
//!
//! * Client disconnects from Lobby and connects to Battle
//!
//! Battle -> all clients: BroadcastPlayerJoin
//!
//! Battle -> Client: BattleStart
//!
//! Client -> Battle: Move
//!
//! Battle -> Client: YouWon
//!
//! * Battle disconnects all players
//!
//! Client -> Lobby: Hello
//!
//! Lobby -> Client: Hello
//!
//! and the loop goes on

use std::net::SocketAddr;

use bevy::prelude::*;
use bincode::DefaultOptions;
use serde::{Deserialize, Serialize};

use bevy_slinet::client::{
    ClientConnection, ClientPlugin, ConnectionEstablishEvent, ConnectionRequestEvent,
};
use bevy_slinet::packet_length_serializer::LittleEndian;
use bevy_slinet::protocols::tcp::TcpProtocol;
use bevy_slinet::protocols::udp::UdpProtocol;
use bevy_slinet::serializers::bincode::BincodeSerializer;
use bevy_slinet::server::{NewConnectionEvent, ServerConnection, ServerPlugin};
use bevy_slinet::{client, server, ClientConfig, ServerConfig};

struct LobbyConfig;

impl ServerConfig for LobbyConfig {
    type ClientPacket = LobbyClientPacket;
    type ServerPacket = LobbyServerPacket;
    type Protocol = TcpProtocol;
    type Serializer = BincodeSerializer<DefaultOptions>;
    type LengthSerializer = LittleEndian<u16>;
}

impl ClientConfig for LobbyConfig {
    type ClientPacket = LobbyClientPacket;
    type ServerPacket = LobbyServerPacket;
    type Protocol = TcpProtocol;
    type Serializer = BincodeSerializer<DefaultOptions>;
    type LengthSerializer = LittleEndian<u16>;
}

struct BattleConfig;

impl ServerConfig for BattleConfig {
    type ClientPacket = BattleClientPacket;
    type ServerPacket = BattleServerPacket;
    type Protocol = UdpProtocol;
    type Serializer = BincodeSerializer<DefaultOptions>;
    type LengthSerializer = LittleEndian<u16>;
}

impl ClientConfig for BattleConfig {
    type ClientPacket = BattleClientPacket;
    type ServerPacket = BattleServerPacket;
    type Protocol = UdpProtocol;
    type Serializer = BincodeSerializer<DefaultOptions>;
    type LengthSerializer = LittleEndian<u16>;
}

#[derive(Serialize, Deserialize, Debug)]
enum LobbyClientPacket {
    Hello,
    Battle,
}

#[derive(Serialize, Deserialize, Debug)]
enum LobbyServerPacket {
    Hello,
    BattleServer(SocketAddr),
}

#[derive(Serialize, Deserialize, Debug)]
enum BattleClientPacket {
    Move,
}

#[derive(Serialize, Deserialize, Debug)]
enum BattleServerPacket {
    BroadcastPlayerJoin,
    BattleStart,
    YouWon,
}

fn main() {
    App::new()
        .add_plugins(MinimalPlugins)
        // Lobby server
        .add_plugin(ServerPlugin::<LobbyConfig>::bind("127.0.0.1:3000").unwrap())
        .add_system(lobby_server_packet_handler)
        // Battle server
        .add_plugin(ServerPlugin::<BattleConfig>::bind("127.0.0.1:3001").unwrap())
        .add_system(battle_server_packet_handler)
        .add_system(battle_server_accept_new_connections)
        // Lobby client connection
        .add_plugin(ClientPlugin::<LobbyConfig>::connect("127.0.0.1:3000"))
        .add_system(lobby_client_connect_handler)
        .add_system(lobby_client_packet_handler)
        // Battle client connection (doesn't connect immediately)
        .add_plugin(ClientPlugin::<BattleConfig>::new())
        .add_system(battle_client_packet_handler)
        .run();
}

fn lobby_server_packet_handler(
    mut event_reader: EventReader<server::PacketReceiveEvent<LobbyConfig>>,
) {
    for event in event_reader.iter() {
        println!("Client -> Lobby: {:?}", event.packet);
        match event.packet {
            LobbyClientPacket::Hello => {
                event.connection.send(LobbyServerPacket::Hello);
            }
            LobbyClientPacket::Battle => {
                event.connection.send(LobbyServerPacket::BattleServer(
                    "127.0.0.1:3001".parse().unwrap(),
                ));
            }
        }
    }
}

fn battle_server_accept_new_connections(
    mut event_reader: EventReader<NewConnectionEvent<BattleConfig>>,
    connections: Res<Vec<ServerConnection<BattleConfig>>>,
) {
    for event in event_reader.iter() {
        println!("[Battle] We have a new player!");
        // We only have 1 player, but this example shows how to get all connected clients.
        // Note that `connection` doesn't contain the new connection yet.
        for connection in connections.iter() {
            connection.send(BattleServerPacket::BroadcastPlayerJoin);
        }
        event.connection.send(BattleServerPacket::BattleStart);
    }
}

fn battle_server_packet_handler(
    mut event_reader: EventReader<server::PacketReceiveEvent<BattleConfig>>,
) {
    for event in event_reader.iter() {
        println!("Client -> Battle: {:?}", event.packet);
        match event.packet {
            BattleClientPacket::Move => {
                event.connection.send(BattleServerPacket::YouWon);
            }
        }
    }
}

fn lobby_client_packet_handler(
    mut event_reader: EventReader<client::PacketReceiveEvent<LobbyConfig>>,
    mut event_writer: EventWriter<ConnectionRequestEvent<BattleConfig>>,
) {
    for event in event_reader.iter() {
        println!("Lobby -> Client: {:?}", event.packet);
        match event.packet {
            LobbyServerPacket::Hello => {
                event.connection.send(LobbyClientPacket::Battle);
            }
            LobbyServerPacket::BattleServer(address) => {
                // Disconnect from the lobby server
                println!("Disconnecting from the lobby server");
                event.connection.disconnect();
                // Connect to the battle server
                println!("Connecting to the battle server");
                event_writer.send(ConnectionRequestEvent::new(address));
            }
        }
    }
}

fn battle_client_packet_handler(
    mut commands: Commands,
    mut event_reader: EventReader<client::PacketReceiveEvent<BattleConfig>>,
    mut event_writer: EventWriter<ConnectionRequestEvent<LobbyConfig>>,
) {
    for event in event_reader.iter() {
        println!("Battle -> Client: {:?}", event.packet);
        match event.packet {
            BattleServerPacket::BroadcastPlayerJoin => {
                // This will never be printed because we only have 1 client.
                println!("[Client] Someone joined the battle");
            }
            BattleServerPacket::BattleStart => {
                event.connection.send(BattleClientPacket::Move);
            }
            BattleServerPacket::YouWon => {
                println!("[Client] I won!");
                commands.remove_resource::<ClientConnection<BattleConfig>>();
                event_writer.send(ConnectionRequestEvent::new("127.0.0.1:3000"));
            }
        }
    }
}

fn lobby_client_connect_handler(mut events: EventReader<ConnectionEstablishEvent<LobbyConfig>>) {
    for event in events.iter() {
        event.connection.send(LobbyClientPacket::Hello)
    }
}
