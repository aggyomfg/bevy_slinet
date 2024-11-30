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
use std::sync::Arc;
use std::time::Duration;

use bevy::log::LogPlugin;
use bevy::prelude::*;
use bevy::time::common_conditions::on_timer;
use bevy_slinet::serializer::SerializerAdapter;
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
    type SerializerError = bincode::Error;
    fn build_serializer(
    ) -> SerializerAdapter<Self::ClientPacket, Self::ServerPacket, Self::SerializerError> {
        SerializerAdapter::ReadOnly(Arc::new(BincodeSerializer::<DefaultOptions>::default()))
    }
    type LengthSerializer = LittleEndian<u16>;
}

impl ClientConfig for LobbyConfig {
    type ClientPacket = LobbyClientPacket;
    type ServerPacket = LobbyServerPacket;
    type Protocol = TcpProtocol;
    type SerializerError = bincode::Error;
    fn build_serializer(
    ) -> SerializerAdapter<Self::ServerPacket, Self::ClientPacket, Self::SerializerError> {
        SerializerAdapter::ReadOnly(Arc::new(BincodeSerializer::<DefaultOptions>::default()))
    }
    type LengthSerializer = LittleEndian<u16>;
}

struct BattleConfig;

impl ServerConfig for BattleConfig {
    type ClientPacket = BattleClientPacket;
    type ServerPacket = BattleServerPacket;
    type Protocol = UdpProtocol;
    type SerializerError = bincode::Error;
    fn build_serializer(
    ) -> SerializerAdapter<Self::ClientPacket, Self::ServerPacket, Self::SerializerError> {
        SerializerAdapter::ReadOnly(Arc::new(BincodeSerializer::<DefaultOptions>::default()))
    }
    type LengthSerializer = LittleEndian<u16>;
}

impl ClientConfig for BattleConfig {
    type ClientPacket = BattleClientPacket;
    type ServerPacket = BattleServerPacket;
    type Protocol = UdpProtocol;
    type SerializerError = bincode::Error;
    fn build_serializer(
    ) -> SerializerAdapter<Self::ServerPacket, Self::ClientPacket, Self::SerializerError> {
        SerializerAdapter::ReadOnly(Arc::new(BincodeSerializer::<DefaultOptions>::default()))
    }
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

fn main() {
    App::new()
        .add_plugins((LogPlugin::default(), MinimalPlugins))
        // Lobby server
        .add_plugins(ServerPlugin::<LobbyConfig>::bind(LOBBY_SERVER))
        .add_observer(lobby_server_accept_new_connections)
        .add_observer(lobby_server_packet_handler)
        // Battle server
        .add_plugins(ServerPlugin::<BattleConfig>::bind(BATTLE_SERVER))
        .add_observer(battle_server_accept_new_connections)
        .add_observer(battle_server_packet_handler)
        // Keep-alive packets
        .init_resource::<ClientKeepAliveTimeout>()
        .init_resource::<ServerKeepAliveMap<LobbyConfig>>()
        .init_resource::<ServerKeepAliveMap<BattleConfig>>()
        .add_systems(
            Update,
            (
                client_send_keepalive.run_if(on_timer(Duration::from_secs_f32(0.5))),
                server_send_keepalive.run_if(on_timer(Duration::from_secs_f32(0.5))),
                client_check_timeout,
                server_tick_keepalive_timers,
            ),
        )
        // Lobby client
        .add_plugins(ClientPlugin::<LobbyConfig>::connect(LOBBY_SERVER))
        .add_observer(lobby_client_connect_handler)
        .add_observer(lobby_client_packet_handler)
        // Battle client (doesn't connect immediately)
        .add_plugins(ClientPlugin::<BattleConfig>::new())
        .add_observer(battle_client_connect_handler)
        .add_observer(battle_client_packet_handler)
        // Reconnection handlers
        .add_observer(lobby_client_reconnect_if_error)
        .add_observer(battle_client_reconnect_if_error)
        .add_observer(lobby_client_keepalive_handler)
        .add_observer(battle_client_keepalive_handler)
        .add_observer(lobby_server_keepalive_handler)
        .add_observer(battle_server_keepalive_handler)
        .run();
}

fn lobby_server_packet_handler(lobby_packet: Trigger<server::PacketReceiveEvent<LobbyConfig>>) {
    let event = lobby_packet.event();
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

fn lobby_server_accept_new_connections(
    new_connection: Trigger<NewConnectionEvent<LobbyConfig>>,
    mut keep_alive_map: ResMut<ServerKeepAliveMap<LobbyConfig>>,
) {
    keep_alive_map.map.insert(
        new_connection.event().connection.id(),
        Timer::from_seconds(1.0, TimerMode::Once),
    );
}

fn battle_server_accept_new_connections(
    new_connection: Trigger<NewConnectionEvent<BattleConfig>>,
    mut keep_alive_map: ResMut<ServerKeepAliveMap<BattleConfig>>,
    connections: Res<ServerConnections<BattleConfig>>,
) {
    let event = new_connection.event();
    log::info!("[Battle] We have a new player!");
    keep_alive_map.map.insert(
        event.connection.id(),
        Timer::from_seconds(1.0, TimerMode::Once),
    );

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

fn battle_server_packet_handler(packet: Trigger<server::PacketReceiveEvent<BattleConfig>>) {
    let event = packet.event();
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

fn lobby_client_packet_handler(
    packet: Trigger<client::PacketReceiveEvent<LobbyConfig>>,
    mut commands: Commands,
) {
    let event = packet.event();
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
            log::info!("Disconnecting from the lobby server");
            event.connection.disconnect();
            log::info!("Connecting to the battle server");
            commands.trigger(ConnectionRequestEvent::<BattleConfig>::new(address));
        }
        _ => (),
    }
}

fn battle_client_packet_handler(
    packet: Trigger<client::PacketReceiveEvent<BattleConfig>>,
    mut commands: Commands,
) {
    let event = packet.event();
    if event.packet != BattleServerPacket::KeepAlive {
        log::info!(
            "Battle -> Client{:?}: {:?}",
            event.connection.id(),
            event.packet
        );
    }
    match event.packet {
        BattleServerPacket::BroadcastPlayerJoin => {
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

            commands.trigger(ConnectionRequestEvent::<LobbyConfig>::new(LOBBY_SERVER));
        }
        _ => (),
    }
}

fn lobby_client_connect_handler(
    connection: Trigger<ConnectionEstablishEvent<LobbyConfig>>,
    mut timeout: ResMut<ClientKeepAliveTimeout>,
) {
    connection
        .event()
        .connection
        .send(LobbyClientPacket::Hello)
        .unwrap();
    timeout.0.reset();
}

fn battle_client_connect_handler(
    _connection: Trigger<ConnectionEstablishEvent<BattleConfig>>,
    mut timeout: ResMut<ClientKeepAliveTimeout>,
) {
    timeout.0.reset();
}

fn lobby_client_keepalive_handler(
    packet: Trigger<client::PacketReceiveEvent<LobbyConfig>>,
    mut timeout: ResMut<ClientKeepAliveTimeout>,
) {
    if packet.event().packet == LobbyServerPacket::KeepAlive {
        timeout.0.reset();
    }
}

fn battle_client_keepalive_handler(
    packet: Trigger<client::PacketReceiveEvent<BattleConfig>>,
    mut timeout: ResMut<ClientKeepAliveTimeout>,
) {
    if packet.event().packet == BattleServerPacket::KeepAlive {
        timeout.0.reset();
    }
}

fn client_check_timeout(
    time: Res<Time>,
    lobby_connection: Option<Res<ClientConnection<LobbyConfig>>>,
    battle_connection: Option<Res<ClientConnection<BattleConfig>>>,
    mut timeout: ResMut<ClientKeepAliveTimeout>,
    mut commands: Commands,
) {
    if timeout.0.tick(time.delta()).just_finished() {
        log::error!("Client timeout");
        if let Some(lobby) = lobby_connection {
            log::error!("Reconnecting to lobby");
            lobby.disconnect();
            commands.trigger(ConnectionRequestEvent::<LobbyConfig>::new(
                lobby.peer_addr(),
            ));
        }
        if let Some(battle) = battle_connection {
            log::error!("Reconnecting to battle");
            battle.disconnect();
            commands.trigger(ConnectionRequestEvent::<BattleConfig>::new(
                battle.peer_addr(),
            ));
        }
    }
}

fn lobby_client_reconnect_if_error(
    disconnect: Trigger<client::DisconnectionEvent<LobbyConfig>>,
    mut commands: Commands,
) {
    let event = disconnect.event();
    if !matches!(event.error, ReceiveError::IntentionalDisconnection) {
        log::error!("Lobby disconnect. Reconnecting. Error: {:?}", event.error);
        commands.trigger(ConnectionRequestEvent::<LobbyConfig>::new(event.address));
    }
}

fn battle_client_reconnect_if_error(
    disconnect: Trigger<client::DisconnectionEvent<BattleConfig>>,
    mut commands: Commands,
) {
    let event = disconnect.event();
    if !matches!(event.error, ReceiveError::IntentionalDisconnection) {
        log::error!("Battle disconnect. Reconnecting. Error: {:?}", event.error);
        commands.trigger(ConnectionRequestEvent::<BattleConfig>::new(event.address));
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

fn lobby_server_keepalive_handler(
    packet: Trigger<server::PacketReceiveEvent<LobbyConfig>>,
    mut lobby_map: ResMut<ServerKeepAliveMap<LobbyConfig>>,
) {
    let event = packet.event();
    if event.packet == LobbyClientPacket::KeepAlive {
        println!("KeepAlive from {:?}", event.connection.id());
        if let Some(keep_alive) = lobby_map.map.get_mut(&event.connection.id()) {
            keep_alive.reset();
        }
    }
}

fn battle_server_keepalive_handler(
    packet: Trigger<server::PacketReceiveEvent<BattleConfig>>,
    mut battle_map: ResMut<ServerKeepAliveMap<BattleConfig>>,
) {
    let event = packet.event();
    if event.packet == BattleClientPacket::KeepAlive {
        println!("KeepAlive from {:?}", event.connection.id());
        if let Some(keep_alive) = battle_map.map.get_mut(&event.connection.id()) {
            keep_alive.reset();
        }
    }
}

fn server_tick_keepalive_timers(
    time: Res<Time>,
    lobby_connections: Res<ServerConnections<LobbyConfig>>,
    mut lobby_map: ResMut<ServerKeepAliveMap<LobbyConfig>>,
    battle_connections: Res<ServerConnections<BattleConfig>>,
    mut battle_map: ResMut<ServerKeepAliveMap<BattleConfig>>,
) {
    for connection in lobby_connections.iter() {
        if let Some(keep_alive) = lobby_map.map.get_mut(&connection.id()) {
            if keep_alive.tick(time.delta()).just_finished() {
                connection.disconnect();
            }
        }
    }

    for connection in battle_connections.iter() {
        if let Some(keep_alive) = battle_map.map.get_mut(&connection.id()) {
            if keep_alive.tick(time.delta()).just_finished() {
                connection.disconnect();
            }
        }
    }
}
