//! Cycles a client from a TCP lobby to a UDP battle and back, with keepalive timeouts
//! and reconnection after unexpected disconnects.

use std::collections::HashMap;
use std::convert::Infallible;
use std::marker::PhantomData;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use bevy::ecs::error::{ResultSeverityExt, Severity};
use bevy::ecs::system::SystemParam;
use bevy::log::{self, LogPlugin};
use bevy::prelude::*;
use bevy::time::common_conditions::on_timer;
use bevy_slinet::serializer::SerializerAdapter;

use bevy_slinet::client::{
    ClientConnection, ClientPlugin, ConnectionEstablishEvent, ConnectionRequestEvent,
};
use bevy_slinet::connection::ConnectionId;
use bevy_slinet::packet_length_serializer::LittleEndian;
use bevy_slinet::protocol::ReceiveError;
use bevy_slinet::protocols::tcp::TcpProtocol;
use bevy_slinet::protocols::udp::UdpProtocol;
use bevy_slinet::serializers::bitcode::BitcodeSerializer;
use bevy_slinet::server::{NewConnectionEvent, ServerConnections, ServerPlugin};
use bevy_slinet::{client, server, ClientConfig, ServerConfig};
use bitcode::{Decode, Encode};

const LOBBY_SERVER: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::LOCALHOST), 3000);
const BATTLE_SERVER: SocketAddr = LOBBY_SERVER;

/// Paces the demonstration so repeated reconnects do not dominate the app loop.
const RECONNECT_DELAY: Duration = Duration::from_millis(100);

struct LobbyConfig;

impl ServerConfig for LobbyConfig {
    type ClientPacket = LobbyClientPacket;
    type ServerPacket = LobbyServerPacket;
    type Protocol = TcpProtocol;
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
    type LengthSerializer = LittleEndian<u16>;
}

impl ClientConfig for LobbyConfig {
    type ClientPacket = LobbyClientPacket;
    type ServerPacket = LobbyServerPacket;
    type Protocol = TcpProtocol;
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
    type LengthSerializer = LittleEndian<u16>;
}

struct BattleConfig;

impl ServerConfig for BattleConfig {
    type ClientPacket = BattleClientPacket;
    type ServerPacket = BattleServerPacket;
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
    type LengthSerializer = LittleEndian<u16>;
}

impl ClientConfig for BattleConfig {
    type ClientPacket = BattleClientPacket;
    type ServerPacket = BattleServerPacket;
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
    type LengthSerializer = LittleEndian<u16>;
}

#[derive(Debug, Decode, Encode, PartialEq)]
enum LobbyClientPacket {
    Hello,
    Battle,
    KeepAlive,
}

#[derive(Debug, Decode, Encode, PartialEq)]
enum LobbyServerPacket {
    Hello,
    BattleServer(SocketAddr),
    KeepAlive,
}

#[derive(Debug, Decode, Encode, PartialEq)]
enum BattleClientPacket {
    Play,
    KeepAlive,
}

#[derive(Debug, Decode, Encode, PartialEq)]
enum BattleServerPacket {
    BroadcastPlayerJoin,
    BattleStart,
    YouWon,
    KeepAlive,
}

#[derive(Resource)]
struct ServerKeepAliveTimers<Config: ServerConfig> {
    timers: HashMap<ConnectionId, Timer>,
    _marker: PhantomData<Config>,
}

impl<Config: ServerConfig> Default for ServerKeepAliveTimers<Config> {
    fn default() -> Self {
        Self {
            timers: HashMap::new(),
            _marker: PhantomData,
        }
    }
}

impl<Config: ServerConfig> ServerKeepAliveTimers<Config> {
    fn track(&mut self, connection: ConnectionId) {
        self.timers.insert(
            connection,
            Timer::new(Duration::from_secs(1), TimerMode::Once),
        );
    }

    fn refresh(&mut self, connection: ConnectionId) {
        if let Some(timer) = self.timers.get_mut(&connection) {
            timer.reset();
        }
    }

    /// An untracked connection never expires through the keepalive timeout.
    fn just_expired(&mut self, connection: ConnectionId, elapsed: Duration) -> bool {
        self.timers
            .get_mut(&connection)
            .is_some_and(|timer| timer.tick(elapsed).just_finished())
    }
}

#[derive(Resource)]
struct ClientKeepAliveTimeout(Timer);

impl Default for ClientKeepAliveTimeout {
    fn default() -> Self {
        Self(Timer::new(Duration::from_secs(5), TimerMode::Once))
    }
}

impl ClientKeepAliveTimeout {
    fn refresh(&mut self) {
        self.0.reset();
    }

    fn just_expired(&mut self, elapsed: Duration) -> bool {
        self.0.tick(elapsed).just_finished()
    }
}

/// Checks keepalive deadlines only for connections still registered with the server.
#[derive(SystemParam)]
struct ServerLiveness<'w, Config: ServerConfig> {
    connections: Res<'w, ServerConnections<Config>>,
    timers: ResMut<'w, ServerKeepAliveTimers<Config>>,
}

impl<Config: ServerConfig> ServerLiveness<'_, Config> {
    fn check_timeouts(&mut self, elapsed: Duration) {
        for connection in self.connections.iter() {
            if self.timers.just_expired(connection.id(), elapsed) {
                connection.disconnect();
            }
        }
    }
}

/// Reads the lobby and battle connections during the client's server handoff.
#[derive(SystemParam)]
struct ActiveServers<'w> {
    lobby: Option<Res<'w, ClientConnection<LobbyConfig>>>,
    battle: Option<Res<'w, ClientConnection<BattleConfig>>>,
}

impl ActiveServers<'_> {
    fn send_keepalive(&self) -> Result {
        match (&self.lobby, &self.battle) {
            (Some(connection), None) => connection
                .send(LobbyClientPacket::KeepAlive)
                .with_severity(Severity::Error)?,
            (None, Some(connection)) => connection
                .send(BattleClientPacket::KeepAlive)
                .with_severity(Severity::Error)?,
            _ => (),
        }
        Ok(())
    }

    fn reconnect(&self, commands: &mut Commands) {
        if let Some(lobby) = &self.lobby {
            log::error!("Reconnecting to lobby");
            lobby.disconnect();
            commands.trigger(ConnectionRequestEvent::<LobbyConfig>::new(
                lobby.peer_addr(),
            ));
        }
        if let Some(battle) = &self.battle {
            log::error!("Reconnecting to battle");
            battle.disconnect();
            commands.trigger(ConnectionRequestEvent::<BattleConfig>::new(
                battle.peer_addr(),
            ));
        }
    }
}

fn main() {
    App::new()
        .add_plugins((LogPlugin::default(), MinimalPlugins))
        .add_plugins(ServerPlugin::<LobbyConfig>::bind(LOBBY_SERVER))
        .add_observer(lobby_server_accept_new_connections)
        .add_observer(lobby_server_packet_handler)
        .add_plugins(ServerPlugin::<BattleConfig>::bind(BATTLE_SERVER))
        .add_observer(battle_server_accept_new_connections)
        .add_observer(battle_server_packet_handler)
        .init_resource::<ClientKeepAliveTimeout>()
        .init_resource::<ServerKeepAliveTimers<LobbyConfig>>()
        .init_resource::<ServerKeepAliveTimers<BattleConfig>>()
        .add_systems(
            Update,
            (
                (|servers: ActiveServers| servers.send_keepalive())
                    .run_if(on_timer(Duration::from_millis(500))),
                server_send_keepalive.run_if(on_timer(Duration::from_millis(500))),
                client_check_timeout,
                |mut server: ServerLiveness<LobbyConfig>, time: Res<Time>| {
                    server.check_timeouts(time.delta());
                },
                |mut server: ServerLiveness<BattleConfig>, time: Res<Time>| {
                    server.check_timeouts(time.delta());
                },
            ),
        )
        .add_plugins(ClientPlugin::<LobbyConfig>::connect(LOBBY_SERVER))
        .add_observer(lobby_client_connect_handler)
        .add_observer(lobby_client_packet_handler)
        .add_plugins(ClientPlugin::<BattleConfig>::new())
        .add_observer(battle_client_connect_handler)
        .add_observer(battle_client_packet_handler)
        .add_observer(lobby_client_reconnect_if_error)
        .add_observer(battle_client_reconnect_if_error)
        .add_observer(lobby_client_keepalive_handler)
        .add_observer(battle_client_keepalive_handler)
        .add_observer(lobby_server_keepalive_handler)
        .add_observer(battle_server_keepalive_handler)
        .run();
}

fn lobby_server_packet_handler(
    lobby_packet: On<server::PacketReceiveEvent<LobbyConfig>>,
) -> Result {
    let event = lobby_packet.event();
    log::info!("Client -> Lobby: {:?}", event.packet);
    match event.packet {
        LobbyClientPacket::Hello => {
            event
                .connection
                .send(LobbyServerPacket::Hello)
                .with_severity(Severity::Error)?;
        }
        LobbyClientPacket::Battle => {
            event
                .connection
                .send(LobbyServerPacket::BattleServer(BATTLE_SERVER))
                .with_severity(Severity::Error)?;
        }
        _ => (),
    }
    Ok(())
}

fn lobby_server_accept_new_connections(
    new_connection: On<NewConnectionEvent<LobbyConfig>>,
    mut keepalive: ResMut<ServerKeepAliveTimers<LobbyConfig>>,
) {
    keepalive.track(new_connection.event().connection.id());
}

fn battle_server_accept_new_connections(
    new_connection: On<NewConnectionEvent<BattleConfig>>,
    mut keepalive: ResMut<ServerKeepAliveTimers<BattleConfig>>,
    connections: Res<ServerConnections<BattleConfig>>,
) -> Result {
    let event = new_connection.event();
    log::info!("[Battle] We have a new player!");
    keepalive.track(event.connection.id());

    for connection in connections.iter() {
        if let Err(error) = connection.send(BattleServerPacket::BroadcastPlayerJoin) {
            log::error!(
                "Failed to announce player join to {:?}: {error}",
                connection.id()
            );
        }
    }
    event
        .connection
        .send(BattleServerPacket::BattleStart)
        .with_severity(Severity::Error)
}

fn battle_server_packet_handler(packet: On<server::PacketReceiveEvent<BattleConfig>>) -> Result {
    let event = packet.event();
    log::info!("Client -> Battle: {:?}", event.packet);
    if event.packet == BattleClientPacket::Play {
        let sent = event.connection.send(BattleServerPacket::YouWon);
        event.connection.disconnect();
        sent.with_severity(Severity::Error)?;
    }
    Ok(())
}

fn lobby_client_packet_handler(
    packet: On<client::PacketReceiveEvent<LobbyConfig>>,
    mut commands: Commands,
) -> Result {
    let event = packet.event();
    log::info!(
        "Lobby -> Client{:?}: {:?}",
        event.connection.id(),
        event.packet
    );
    match event.packet {
        LobbyServerPacket::Hello => {
            event
                .connection
                .send(LobbyClientPacket::Battle)
                .with_severity(Severity::Error)?;
        }
        LobbyServerPacket::BattleServer(address) => {
            log::info!("Disconnecting from the lobby server");
            event.connection.disconnect();
            log::info!("Connecting to the battle server");
            commands.trigger(ConnectionRequestEvent::<BattleConfig>::new(address));
        }
        _ => (),
    }
    Ok(())
}

fn battle_client_packet_handler(
    packet: On<client::PacketReceiveEvent<BattleConfig>>,
    mut commands: Commands,
) -> Result {
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
            event
                .connection
                .send(BattleClientPacket::Play)
                .with_severity(Severity::Error)?;
        }
        BattleServerPacket::YouWon => {
            log::info!("[Client] I won!");
            event.connection.disconnect();

            std::thread::sleep(RECONNECT_DELAY);

            commands.trigger(ConnectionRequestEvent::<LobbyConfig>::new(LOBBY_SERVER));
        }
        _ => (),
    }
    Ok(())
}

fn lobby_client_connect_handler(
    connection: On<ConnectionEstablishEvent<LobbyConfig>>,
    mut timeout: ResMut<ClientKeepAliveTimeout>,
) -> Result {
    timeout.refresh();
    connection
        .event()
        .connection
        .send(LobbyClientPacket::Hello)
        .with_severity(Severity::Error)?;
    Ok(())
}

fn battle_client_connect_handler(
    _connection: On<ConnectionEstablishEvent<BattleConfig>>,
    mut timeout: ResMut<ClientKeepAliveTimeout>,
) {
    timeout.refresh();
}

fn lobby_client_keepalive_handler(
    packet: On<client::PacketReceiveEvent<LobbyConfig>>,
    mut timeout: ResMut<ClientKeepAliveTimeout>,
) {
    if packet.event().packet == LobbyServerPacket::KeepAlive {
        timeout.refresh();
    }
}

fn battle_client_keepalive_handler(
    packet: On<client::PacketReceiveEvent<BattleConfig>>,
    mut timeout: ResMut<ClientKeepAliveTimeout>,
) {
    if packet.event().packet == BattleServerPacket::KeepAlive {
        timeout.refresh();
    }
}

fn client_check_timeout(
    time: Res<Time>,
    servers: ActiveServers,
    mut timeout: ResMut<ClientKeepAliveTimeout>,
    mut commands: Commands,
) {
    if timeout.just_expired(time.delta()) {
        log::error!("Client timeout");
        servers.reconnect(&mut commands);
    }
}

fn lobby_client_reconnect_if_error(
    disconnect: On<client::DisconnectionEvent<LobbyConfig>>,
    mut commands: Commands,
) {
    let event = disconnect.event();
    if !matches!(event.error, ReceiveError::IntentionalDisconnection) {
        log::error!("Lobby disconnect. Reconnecting. Error: {:?}", event.error);
        commands.trigger(ConnectionRequestEvent::<LobbyConfig>::new(event.address));
    }
}

fn battle_client_reconnect_if_error(
    disconnect: On<client::DisconnectionEvent<BattleConfig>>,
    mut commands: Commands,
) {
    let event = disconnect.event();
    if !matches!(event.error, ReceiveError::IntentionalDisconnection) {
        log::error!("Battle disconnect. Reconnecting. Error: {:?}", event.error);
        commands.trigger(ConnectionRequestEvent::<BattleConfig>::new(event.address));
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
    packet: On<server::PacketReceiveEvent<LobbyConfig>>,
    mut keepalive: ResMut<ServerKeepAliveTimers<LobbyConfig>>,
) {
    let event = packet.event();
    if event.packet == LobbyClientPacket::KeepAlive {
        println!("KeepAlive from {:?}", event.connection.id());
        keepalive.refresh(event.connection.id());
    }
}

fn battle_server_keepalive_handler(
    packet: On<server::PacketReceiveEvent<BattleConfig>>,
    mut keepalive: ResMut<ServerKeepAliveTimers<BattleConfig>>,
) {
    let event = packet.event();
    if event.packet == BattleClientPacket::KeepAlive {
        println!("KeepAlive from {:?}", event.connection.id());
        keepalive.refresh(event.connection.id());
    }
}
