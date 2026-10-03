//! A finite UDP handshake implemented entirely by the application.
//!
//! Run with `cargo run --example udp_application_sessions --all-features`.
//! A normal Bevy App runs both endpoints in this process. `ClientSessions` and
//! `ServerSessions` resources own handshake and liveness state. Update systems
//! retry requests and expire peers; observers process application packets.
//! Two logical sessions share one UDP peer, then are replaced and closed.
//! This demonstrates lifecycle, not authentication or encryption. Production
//! applications must choose their own admission, identity and replay policies.

#[path = "udp_application_sessions/session.rs"]
mod session;

use bevy::{platform::time::Instant, prelude::*};
use bevy_slinet::{
    client::{self, ClientConnection, ClientPlugin, ConnectionEstablishEvent},
    connection::MaxPacketSize,
    protocols::udp::{ConfiguredUdpProtocol, UdpConfig, UdpOptions},
    serializers::{
        bitcode::BitcodeSerializer, packet_length_serializer::LittleEndian,
        serializer::SerializerAdapter,
    },
    server::{self, NewConnectionEvent, ServerAddress, ServerConnection, ServerPlugin},
    ClientConfig, ServerConfig,
};
use session::{Body, Packet, Sessions};
use std::{
    collections::HashMap, convert::Infallible, io, net::SocketAddr, sync::Arc, time::Duration,
};

const RETRY: Duration = Duration::from_millis(50);
const IDLE: Duration = Duration::from_millis(500);
const DEADLINE: Duration = Duration::from_secs(5);
const MAX_PEERS: usize = 64;
struct Config;
impl UdpConfig for Config {
    const OPTIONS: UdpOptions = UdpOptions {
        max_peers: MAX_PEERS,
        ..UdpOptions::DEFAULT
    };
}
impl ClientConfig for Config {
    type ClientPacket = Packet;
    type ServerPacket = Packet;
    type Protocol = ConfiguredUdpProtocol<Self>;
    type EncodeError = Infallible;
    type DecodeError = bitcode::Error;
    type LengthSerializer = LittleEndian<u32>; // Ignored by UDP.
    fn build_serializer() -> SerializerAdapter<Packet, Packet, Infallible, bitcode::Error> {
        SerializerAdapter::ReadOnly(Arc::new(BitcodeSerializer))
    }
}
impl ServerConfig for Config {
    type ClientPacket = Packet;
    type ServerPacket = Packet;
    type Protocol = ConfiguredUdpProtocol<Self>;
    type EncodeError = Infallible;
    type DecodeError = bitcode::Error;
    type LengthSerializer = LittleEndian<u32>;
    fn build_serializer() -> SerializerAdapter<Packet, Packet, Infallible, bitcode::Error> {
        SerializerAdapter::ReadOnly(Arc::new(BitcodeSerializer))
    }
}

struct Peer {
    connection: ServerConnection<Config>,
    sessions: Sessions,
    created: Instant,
}
#[derive(Resource, Default)]
struct ServerSessions {
    peers: HashMap<SocketAddr, Peer>,
}
fn new_peer(event: On<NewConnectionEvent<Config>>, mut state: ResMut<ServerSessions>) {
    let address = event.connection.peer_addr();
    if let Some(peer) = state.peers.get_mut(&address) {
        peer.connection = event.connection.clone();
        peer.created = Instant::now();
    } else if state.peers.len() < MAX_PEERS {
        state.peers.insert(
            address,
            Peer {
                connection: event.connection.clone(),
                sessions: Sessions::default(),
                created: Instant::now(),
            },
        );
    } else {
        event.connection.disconnect();
    }
}
fn server_packet(
    event: On<server::PacketReceiveEvent<Config>>,
    mut state: ResMut<ServerSessions>,
) -> Result {
    let packet = &event.packet;
    let Some(peer) = state.peers.get_mut(&event.connection.peer_addr()) else {
        return Ok(());
    };
    if let Some(reply) = peer.sessions.handle(packet, event.received_at) {
        event.connection.send(reply)?;
    }
    Ok(())
}
fn expire_peers(mut state: ResMut<ServerSessions>) {
    let now = Instant::now();
    state.peers.retain(|_, peer| {
        peer.sessions.expire(now, IDLE);
        // Includes peers whose first packet was malformed and never decoded.
        if peer.sessions.active() == 0 && now.saturating_duration_since(peer.created) >= IDLE {
            peer.connection.disconnect();
            // Pending addresses have no replay history and must free admission space.
            return peer.sessions.has_history();
        }
        true
    });
    // Retain only bounded, previously admitted replay records in this finite demo.
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Initial,
    Replacement,
    Echo,
    Closing,
    Done,
}
struct Request {
    old: Packet,
    current: Packet,
    phase: Phase,
    last_sent: Option<Instant>,
}
impl Request {
    fn matches(&self, packet: &Packet) -> bool {
        let expected = if self.phase == Phase::Initial {
            &self.old
        } else {
            &self.current
        };
        packet.channel == expected.channel
            && packet.generation == expected.generation
            && packet.id == expected.id
    }
    fn outgoing(&self) -> Option<Packet> {
        let body = match self.phase {
            Phase::Initial => return Some(self.old.clone()),
            Phase::Replacement => Body::Hello,
            Phase::Echo => Body::Data(b"application data".to_vec()),
            Phase::Closing => Body::Close,
            Phase::Done => return None,
        };
        Some(Packet {
            body,
            ..self.current.clone()
        })
    }
}
#[derive(Resource)]
struct ClientSessions {
    connection: Option<ClientConnection<Config>>,
    requests: Vec<Request>,
    attempts: usize,
}
fn ready(event: On<ConnectionEstablishEvent<Config>>, mut state: ResMut<ClientSessions>) {
    // Local readiness is not a successful application handshake.
    state.connection = Some(event.connection.clone());
}
fn drive_client(mut state: ResMut<ClientSessions>) -> Result {
    let Some(connection) = state.connection.clone() else {
        return Ok(());
    };
    let now = Instant::now();
    let mut attempts = 0;
    for request in &mut state.requests {
        if request
            .last_sent
            .is_some_and(|last| now.saturating_duration_since(last) < RETRY)
        {
            continue;
        }
        if let Some(packet) = request.outgoing() {
            connection.send(packet)?;
            request.last_sent = Some(now);
            attempts += 1;
        }
    }
    state.attempts += attempts;
    Ok(())
}
fn client_packet(event: On<client::PacketReceiveEvent<Config>>, mut state: ResMut<ClientSessions>) {
    let Some(request) = state
        .requests
        .iter_mut()
        .find(|request| request.matches(&event.packet))
    else {
        return;
    };
    match (&event.packet.body, request.phase) {
        (Body::Welcome, Phase::Initial) => {
            request.phase = Phase::Replacement;
        }
        (Body::Welcome, Phase::Replacement) => {
            request.phase = Phase::Echo;
        }
        (Body::Data(data), Phase::Echo) if data == b"application data" => {
            request.phase = Phase::Closing;
        }
        (Body::Close, Phase::Closing) => {
            request.phase = Phase::Done;
        }
        _ => return,
    }
    request.last_sent = None;
}

fn random_id(size: usize) -> io::Result<Vec<u8>> {
    let mut id = vec![0; size];
    getrandom::fill(&mut id).map_err(|err| io::Error::other(err.to_string()))?;
    Ok(id)
}
fn server_app() -> App {
    let mut app = App::new();
    app.insert_resource(MaxPacketSize(1200))
        .init_resource::<ServerSessions>()
        .add_plugins(ServerPlugin::<Config>::bind("127.0.0.1:0"))
        .add_observer(new_peer)
        .add_observer(server_packet)
        .add_systems(Update, expire_peers);
    app
}
fn client_sessions(sizes: &[usize]) -> io::Result<ClientSessions> {
    let requests = sizes
        .iter()
        .enumerate()
        .map(|(channel, &size)| {
            let channel = u8::try_from(channel).map_err(io::Error::other)?;
            Ok(Request {
                old: Packet {
                    channel,
                    generation: 1,
                    id: random_id(size)?,
                    body: Body::Hello,
                },
                current: Packet {
                    channel,
                    generation: 2,
                    id: random_id(size)?,
                    body: Body::Hello,
                },
                phase: Phase::Initial,
                last_sent: None,
            })
        })
        .collect::<io::Result<Vec<_>>>()?;
    Ok(ClientSessions {
        connection: None,
        requests,
        attempts: 0,
    })
}

#[derive(Resource)]
struct DemoStarted(Instant);

fn request_connection(
    address: Res<ServerAddress<Config>>,
    mut requested: Local<bool>,
    mut commands: Commands,
) {
    if !*requested {
        commands.trigger(client::ConnectionRequestEvent::<Config>::new(
            address.address(),
        ));
        *requested = true;
    }
}

fn finish_demo(
    client: Res<ClientSessions>,
    started: Res<DemoStarted>,
    mut exit: MessageWriter<AppExit>,
) {
    if client.connection.is_some()
        && client
            .requests
            .iter()
            .all(|request| request.phase == Phase::Done)
    {
        if let Some(connection) = &client.connection {
            connection.disconnect();
        }
        println!(
            "{} application sessions completed with 48/64-byte IDs in {} sends",
            client.requests.len(),
            client.attempts
        );
        exit.write(AppExit::Success);
    } else if started.0.elapsed() >= DEADLINE {
        bevy::log::error!("Application handshake/exchange timed out");
        exit.write(AppExit::error());
    }
}

fn main() -> io::Result<()> {
    let mut app = server_app();
    app.add_plugins((
        MinimalPlugins.set(bevy::app::ScheduleRunnerPlugin::run_loop(
            Duration::from_millis(10),
        )),
        bevy::log::LogPlugin::default(),
        ClientPlugin::<Config>::new(),
    ))
    .insert_resource(client_sessions(&[48, 64])?)
    .insert_resource(DemoStarted(Instant::now()))
    .add_observer(ready)
    .add_observer(client_packet)
    .add_systems(
        Update,
        (request_connection, drive_client, finish_demo).chain(),
    );
    match app.run() {
        AppExit::Success => Ok(()),
        AppExit::Error(code) => Err(io::Error::other(format!("session demo exited with {code}"))),
    }
}
