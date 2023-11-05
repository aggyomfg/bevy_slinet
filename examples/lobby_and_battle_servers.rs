//! The most complete and feature-rich example that aims to show all bevy_slinet's features.
//! It's a bit complicated because it implements 2 clients and 2 servers, you probably don't
//! want to do this in one `App`, but it's possible. Also it includes KeepAlive system to disconnect
//! timed out players and runtime connection/disconnection/reconnection.
//!
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
//! Client -> Battle: Play
//!
//! Battle -> Client: YouWon
//!
//! * Battle disconnects all players, the client connects to Lobby
//!
//! Client -> Lobby: Hello
//!
//! Lobby -> Client: Hello
//!
//! and the loop goes on

use std::collections::HashMap;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::time::Duration;

use bevy::log::LogPlugin;
use bevy::prelude::*;
use bevy::time::common_conditions::on_timer;
use bincode::DefaultOptions;
use serde::{Deserialize, Serialize};

use bevy_slinet::client::{
    ClientConnection, ClientPlugin, ConnectionEstablishEvent, ConnectionRequestEvent,
};
use bevy_slinet::connection::ConnectionId;
use bevy_slinet::packet_length_serializer::LittleEndian;
use bevy_slinet::protocol::ReceiveError;
use bevy_slinet::protocols::tcp::TcpProtocol;
use bevy_slinet::protocols::udp::UdpProtocol;
use bevy_slinet::serializers::bincode::BincodeSerializer;
use bevy_slinet::server::{NewConnectionEvent, ServerConnections, ServerPlugin};
use bevy_slinet::{client, server, ClientConfig, ServerConfig};

pub const LOBBY_SERVER: &str = "127.0.0.1:3000";
pub const BATTLE_SERVER: &str = "127.0.0.1:3000";

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

#[derive(Serialize, Deserialize, Debug, PartialEq)]
enum LobbyClientPacket {
    Hello,
    Battle,
    KeepAlive,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
enum LobbyServerPacket {
    Hello,
    BattleServer(SocketAddr),
    KeepAlive,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
enum BattleClientPacket {
    Play,
    KeepAlive,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
enum BattleServerPacket {
    BroadcastPlayerJoin,
    BattleStart,
    YouWon,
    KeepAlive,
}

#[derive(Resource)]
struct ServerKeepAliveMap<Config: ServerConfig> {
    map: HashMap<ConnectionId, Timer>,
    _marker: PhantomData<Config>,
}

#[derive(Resource)]
struct ClientKeepAliveTimeout(Timer);

impl Default for ClientKeepAliveTimeout {
    fn default() -> Self {
        ClientKeepAliveTimeout(Timer::from_seconds(5.0, TimerMode::Once))
    }
}

impl<Config: ServerConfig> Default for ServerKeepAliveMap<Config> {
    fn default() -> Self {
        ServerKeepAliveMap {
            map: Default::default(),
            _marker: Default::default(),
        }
    }
}

#[derive(SystemSet, Clone, Hash, Debug, PartialEq, Eq)]
enum SystemSets {
    ServerRemoveTimedOutClients,
}

fn main() {
    App::new()
        .add_plugins((LogPlugin::default(), MinimalPlugins))
        // Lobby server
        .add_plugins(ServerPlugin::<LobbyConfig>::bind(LOBBY_SERVER))
        .add_systems(
            Update,
            (
                lobby_server_accept_new_connections.before(SystemSets::ServerRemoveTimedOutClients),
                lobby_server_packet_handler,
            ),
        )
        // Battle server
        .add_plugins(ServerPlugin::<BattleConfig>::bind(BATTLE_SERVER))
        .add_systems(
            Update,
            (
                battle_server_accept_new_connections
                    .before(SystemSets::ServerRemoveTimedOutClients),
                battle_server_packet_handler,
            ),
        )
        // Keep-alive packets
        .init_resource::<ClientKeepAliveTimeout>()
        .init_resource::<ServerKeepAliveMap<LobbyConfig>>()
        .init_resource::<ServerKeepAliveMap<BattleConfig>>()
        .add_systems(
            Update,
            (
                client_send_keepalive.run_if(on_timer(Duration::from_secs_f32(0.5))),
                server_send_keepalive.run_if(on_timer(Duration::from_secs_f32(0.5))),
                client_reconnect_if_timeout,
                client_reconnect_if_error,
                server_remove_timed_out_clients.in_set(SystemSets::ServerRemoveTimedOutClients),
            ),
        )
        // Lobby client
        .add_plugins(ClientPlugin::<LobbyConfig>::connect(LOBBY_SERVER))
        .add_systems(
            Update,
            (lobby_client_connect_handler, lobby_client_packet_handler),
        )
        // Battle client (doesn't connect immediately)
        .add_plugins(ClientPlugin::<BattleConfig>::new())
        .add_systems(
            Update,
            (battle_client_connect_handler, battle_client_packet_handler),
        )
        .run();
}

fn lobby_server_packet_handler(
    mut event_reader: EventReader<server::PacketReceiveEvent<LobbyConfig>>,
) {
    for event in event_reader.read() {
        log::info!("Client -> Lobby: {:?}", event.packet);
        match event.packet {
            LobbyClientPacket::Hello => {
                event.connection.send(LobbyServerPacket::Hello).unwrap();
            }
            LobbyClientPacket::Battle => {
                event
                    .connection
                    .send(LobbyServerPacket::BattleServer(
                        BATTLE_SERVER.parse().unwrap(),
                    ))
                    .unwrap();
            }
            _ => (),
        }
    }
}

fn lobby_server_accept_new_connections(
    mut event_reader: EventReader<NewConnectionEvent<LobbyConfig>>,
    mut keep_alive_map: ResMut<ServerKeepAliveMap<LobbyConfig>>,
) {
    for event in event_reader.read() {
        keep_alive_map.map.insert(
            event.connection.id(),
            Timer::from_seconds(1.0, TimerMode::Once),
        );
    }
}

fn battle_server_accept_new_connections(
    mut event_reader: EventReader<NewConnectionEvent<BattleConfig>>,
    mut keep_alive_map: ResMut<ServerKeepAliveMap<BattleConfig>>,
    connections: Res<ServerConnections<BattleConfig>>,
) {
    for event in event_reader.read() {
        log::info!("[Battle] We have a new player!");
        keep_alive_map.map.insert(
            event.connection.id(),
            Timer::from_seconds(1.0, TimerMode::Once),
        );

        // We only have 1 player, but this example shows how to get all connected clients.
        // Note that `connection` doesn't contain the new connection yet.
        for connection in connections.iter() {
            connection
                .send(BattleServerPacket::BroadcastPlayerJoin)
                .unwrap();
        }
        event
            .connection
            .send(BattleServerPacket::BattleStart)
            .unwrap();
    }
}

fn battle_server_packet_handler(
    mut event_reader: EventReader<server::PacketReceiveEvent<BattleConfig>>,
) {
    for event in event_reader.read() {
        log::info!("Client -> Battle: {:?}", event.packet);
        #[allow(clippy::single_match)]
        match event.packet {
            BattleClientPacket::Play => {
                event.connection.send(BattleServerPacket::YouWon).unwrap();
                event.connection.disconnect();
            }
            _ => (),
        }
    }
}

fn lobby_client_packet_handler(
    mut event_reader: EventReader<client::PacketReceiveEvent<LobbyConfig>>,
    mut event_writer: EventWriter<ConnectionRequestEvent<BattleConfig>>,
) {
    for event in event_reader.read() {
        log::info!(
            "Lobby -> Client{:?}: {:?}",
            event.connection.id(),
            event.packet
        );
        match event.packet {
            LobbyServerPacket::Hello => {
                event.connection.send(LobbyClientPacket::Battle).unwrap();
            }
            LobbyServerPacket::BattleServer(address) => {
                // Disconnect from the lobby server
                log::info!("Disconnecting from the lobby server");
                event.connection.disconnect();
                // Connect to the battle server
                log::info!("Connecting to the battle server");
                event_writer.send(ConnectionRequestEvent::new(address));
            }
            _ => (),
        }
    }
}

fn battle_client_packet_handler(
    mut event_reader: EventReader<client::PacketReceiveEvent<BattleConfig>>,
    mut event_writer: EventWriter<ConnectionRequestEvent<LobbyConfig>>,
) {
    for event in event_reader.read() {
        if event.packet != BattleServerPacket::KeepAlive {
            log::info!(
                "Battle -> Client{:?}: {:?}",
                event.connection.id(),
                event.packet
            );
        }
        match event.packet {
            BattleServerPacket::BroadcastPlayerJoin => {
                // This will never be printed because we only have 1 client.
                log::info!("[Client] Someone joined the battle");
            }
            BattleServerPacket::BattleStart => {
                event.connection.send(BattleClientPacket::Play).unwrap();
            }
            BattleServerPacket::YouWon => {
                log::info!("[Client] I won!");
                event.connection.disconnect();

                // Delay between connections is required for this particular example
                // because the operating system may assign the same port (we use
                // 127.0.0.1:0 to let OS pick a free port for us) twice before the
                // server disconnects the first client, and will treat a new connection
                // as an existing one. In this example our server is sending a `BattleStart`
                // packet and the client doesn't implement a proper error handling mechanism
                // to prevent situations like "no BattleStart but lot of KeepAlive". Your
                // real case would start with authentication and more complex initial connection
                // logic, as well as reasonable number of connections, not just spamming the
                // server with millions of connections from a single IP. The reason is that the
                // server disconnects the client after 1 second of no keep-alive packets, but
                // sometimes 1 second is enough for second connection to have the same port.
                // You can remove this line and try it yourself (in release mode). After a few
                // millions of connections you'll notice that the entire connection is just
                // KeepAlive packets and nothing else.
                std::thread::sleep(Duration::from_secs_f64(0.10));

                event_writer.send(ConnectionRequestEvent::new(LOBBY_SERVER));
            }
            _ => (),
        }
    }
}

fn lobby_client_connect_handler(
    mut events: EventReader<ConnectionEstablishEvent<LobbyConfig>>,
    mut timeout: ResMut<ClientKeepAliveTimeout>,
) {
    for event in events.read() {
        event.connection.send(LobbyClientPacket::Hello).unwrap();
        timeout.0.reset();
    }
}

fn battle_client_connect_handler(
    mut events: EventReader<ConnectionEstablishEvent<BattleConfig>>,
    mut timeout: ResMut<ClientKeepAliveTimeout>,
) {
    for _ in events.read() {
        timeout.0.reset();
    }
}

fn client_send_keepalive(
    lobby: Option<Res<ClientConnection<LobbyConfig>>>,
    battle: Option<Res<ClientConnection<BattleConfig>>>,
) {
    match (lobby, battle) {
        (Some(conn), None) => conn.send(LobbyClientPacket::KeepAlive).unwrap(),
        (None, Some(conn)) => conn.send(BattleClientPacket::KeepAlive).unwrap(),
        (Some(_), Some(_)) => (),
        (None, None) => (),
    }
}

fn server_send_keepalive(
    lobby: Res<ServerConnections<LobbyConfig>>,
    battle: Res<ServerConnections<BattleConfig>>,
) {
    for client in lobby.iter() {
        let _ = client.send(LobbyServerPacket::KeepAlive);
    }
    for client in battle.iter() {
        let _ = client.send(BattleServerPacket::KeepAlive);
    }
}

#[allow(clippy::too_many_arguments)]
fn client_reconnect_if_timeout(
    time: Res<Time>,

    lobby_connection: Option<Res<ClientConnection<LobbyConfig>>>,
    mut lobby_packets: EventReader<client::PacketReceiveEvent<LobbyConfig>>,
    mut lobby_events: EventWriter<client::ConnectionRequestEvent<LobbyConfig>>,

    battle_connection: Option<Res<ClientConnection<BattleConfig>>>,
    mut battle_packets: EventReader<client::PacketReceiveEvent<BattleConfig>>,
    mut battle_events: EventWriter<client::ConnectionRequestEvent<BattleConfig>>,

    mut timeout: ResMut<ClientKeepAliveTimeout>,
) {
    for packet in lobby_packets.read() {
        if packet.packet == LobbyServerPacket::KeepAlive {
            timeout.0.reset();
        }
    }
    for packet in battle_packets.read() {
        if packet.packet == BattleServerPacket::KeepAlive {
            timeout.0.reset();
        }
    }

    if timeout.0.tick(time.delta()).just_finished() {
        log::error!("Client timeout");
        if let Some(lobby) = lobby_connection {
            log::error!("Reconnecting to lobby");
            lobby.disconnect();
            lobby_events.send(ConnectionRequestEvent::new(lobby.peer_addr()));
        }
        if let Some(battle) = battle_connection {
            log::error!("Reconnecting to battle");
            battle.disconnect();
            battle_events.send(ConnectionRequestEvent::new(battle.peer_addr()));
        }
    }
}

fn client_reconnect_if_error(
    mut lobby_disconnect: EventReader<client::DisconnectionEvent<LobbyConfig>>,
    mut lobby_reconnect: EventWriter<ConnectionRequestEvent<LobbyConfig>>,

    mut battle_disconnect: EventReader<client::DisconnectionEvent<BattleConfig>>,
    mut battle_reconnect: EventWriter<ConnectionRequestEvent<BattleConfig>>,
) {
    for event in lobby_disconnect.read() {
        if !matches!(event.error, ReceiveError::IntentionalDisconnection) {
            log::error!("Lobby disconnect. Reconnecting. Error: {:?}", event.error);
            lobby_reconnect.send(ConnectionRequestEvent::new(event.address));
        }
    }
    for event in battle_disconnect.read() {
        if !matches!(event.error, ReceiveError::IntentionalDisconnection) {
            log::error!("Battle disconnect. Reconnecting. Error: {:?}", event.error);
            battle_reconnect.send(ConnectionRequestEvent::new(event.address));
        }
    }
}

fn server_remove_timed_out_clients(
    time: Res<Time>,

    lobby_connections: Res<ServerConnections<LobbyConfig>>,
    mut lobby_map: ResMut<ServerKeepAliveMap<LobbyConfig>>,
    mut lobby_events: EventReader<server::PacketReceiveEvent<LobbyConfig>>,

    battle_connections: Res<ServerConnections<BattleConfig>>,
    mut battle_map: ResMut<ServerKeepAliveMap<BattleConfig>>,
    mut battle_events: EventReader<server::PacketReceiveEvent<BattleConfig>>,
) {
    for event in lobby_events.read() {
        if event.packet == LobbyClientPacket::KeepAlive {
            lobby_map
                .map
                .get_mut(&event.connection.id())
                .unwrap()
                .reset()
        }
    }
    for connection in &**lobby_connections {
        if lobby_map
            .map
            .get_mut(&connection.id())
            .unwrap()
            .tick(time.delta())
            .just_finished()
        {
            connection.disconnect();
        }
    }

    for event in battle_events.read() {
        if event.packet == BattleClientPacket::KeepAlive {
            battle_map
                .map
                .get_mut(&event.connection.id())
                .unwrap()
                .reset()
        }
    }
    for connection in battle_connections.iter() {
        if battle_map
            .map
            .get_mut(&connection.id())
            .unwrap()
            .tick(time.delta())
            .just_finished()
        {
            connection.disconnect();
        }
    }
}
