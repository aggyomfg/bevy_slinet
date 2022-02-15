//! Server part of the plugin. You can enable it by adding `server` feature.

use core::default::Default;
use std::io::ErrorKind;
use std::marker::PhantomData;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use bevy::prelude::*;
use crossbeam_channel::Receiver;

use crate::connection::{
    max_packet_size_system, ConnectionId, EcsConnection, RawConnection, ReceiveError,
    MAX_PACKET_SIZE,
};
use crate::protocol::{Listener, NetworkStream, Protocol};
use crate::{ServerConfig, SystemLabels};

/// Server-side connection to a server.
pub type ServerConnection<Config> = EcsConnection<<Config as ServerConfig>::ServerPacket>;

/// Server-side plugin. Use [`ServerPlugin::bind`] to create.
pub struct ServerPlugin<Config: ServerConfig> {
    address: SocketAddr,
    _marker: PhantomData<Config>,
}

impl<Config: ServerConfig> Plugin for ServerPlugin<Config> {
    fn build(&self, app: &mut App) {
        if MAX_PACKET_SIZE.load(Ordering::Relaxed) == usize::MAX {
            log::warn!("You haven't set \"MaxPacketSize\" resource! This is a security risk, please insert it before using this server in production.")
        }

        app.add_event::<NewConnectionEvent<Config>>()
            .add_event::<DisconnectionEvent<Config>>()
            .add_event::<PacketReceiveEvent<Config>>()
            .insert_resource(Vec::<ServerConnection<Config>>::new())
            .add_startup_system(create_setup_system::<Config>(self.address))
            .add_system(max_packet_size_system.label(SystemLabels::SetMaxPacketSize))
            .add_system_to_stage(
                CoreStage::PreUpdate,
                accept_new_connections::<Config>.label(SystemLabels::ServerAcceptNewConnections),
            )
            .add_system_to_stage(
                CoreStage::PreUpdate,
                accept_new_packets::<Config>.label(SystemLabels::ServerAcceptNewPackets),
            )
            .add_system_to_stage(
                CoreStage::PreUpdate,
                remove_connections::<Config>.label(SystemLabels::ServerRemoveConnections),
            )
            .add_system(connection_add_system::<Config>.label(SystemLabels::ServerConnectionAdd));
    }
}

impl<Config: ServerConfig> ServerPlugin<Config> {
    /// Bind to the specified address and return a [`ServerPlugin`].
    pub fn bind<A>(address: A) -> std::io::Result<ServerPlugin<Config>>
    where
        A: ToSocketAddrs,
    {
        Ok(ServerPlugin {
            address: address
                .to_socket_addrs()
                .expect("Invalid address")
                .next()
                .expect("Invalid address"),
            _marker: PhantomData,
        })
    }
}

struct ConnectionReceiver<Config: ServerConfig>(Receiver<(SocketAddr, ServerConnection<Config>)>);
struct DisconnectionReceiver<Config: ServerConfig>(Receiver<ServerConnection<Config>>);
struct PacketReceiver<Config: ServerConfig>(
    Receiver<(ServerConnection<Config>, Config::ClientPacket)>,
);

fn create_setup_system<Config: ServerConfig>(address: SocketAddr) -> impl Fn(Commands) {
    move |mut commands: Commands| {
        let (conn_tx, conn_rx) = crossbeam_channel::unbounded();
        let (conn_tx2, conn_rx2) = crossbeam_channel::unbounded();
        let (disc_tx, disc_rx) = crossbeam_channel::unbounded();
        let (pack_tx, pack_rx) = crossbeam_channel::unbounded();
        commands.insert_resource(ConnectionReceiver::<Config>(conn_rx));
        commands.insert_resource(DisconnectionReceiver::<Config>(disc_rx));
        commands.insert_resource(PacketReceiver::<Config>(pack_rx));

        // Listener
        std::thread::spawn(move || {
            let listener =
                Config::Protocol::create_listener(address).expect("Couldn't create listener");

            loop {
                while let Ok((connection, address)) = listener.accept() {
                    connection.set_nonblocking();
                    log::debug!("Accepting a connection from {:?}", address);
                    let (tx, rx) = crossbeam_channel::unbounded();
                    let connection = RawConnection {
                        stream: connection,
                        serializer: Arc::new(Default::default()),
                        packet_length_serializer: Arc::new(Default::default()),
                        id: ConnectionId::next(),
                        packets_rx: rx,
                        _receive_packet: PhantomData,
                        _send_packet: PhantomData,
                    };
                    let ecs_conn = EcsConnection {
                        id: connection.id(),
                        packet_tx: tx,
                        local_addr: connection.local_addr(),
                        peer_addr: connection.peer_addr(),
                    };
                    conn_tx.send((address, ecs_conn.clone())).unwrap();
                    conn_tx2.send((connection, ecs_conn)).unwrap();
                }
            }
        });

        // Packets
        std::thread::spawn(move || {
            #[allow(clippy::type_complexity)]
            let mut connections: Vec<(
                RawConnection<
                    Config::ClientPacket,
                    Config::ServerPacket,
                    <Config::Protocol as Protocol>::ServerStream,
                    Config::Serializer,
                    Config::LengthSerializer,
                >,
                EcsConnection<Config::ServerPacket>,
            )> = Vec::new();
            loop {
                for new_connection in conn_rx2.try_iter() {
                    connections.push(new_connection);
                }
                let mut to_remove = Vec::new();
                for (connection, ecs_conn) in connections.iter_mut() {
                    match connection.receive() {
                        Ok(packet) => {
                            log::trace!("Received packet {:?}", packet);
                            pack_tx.send((ecs_conn.clone(), packet)).unwrap();
                        }
                        Err(ReceiveError::Io(err))
                            if matches!(
                                err.kind(),
                                ErrorKind::TimedOut
                                    | ErrorKind::ConnectionAborted
                                    | ErrorKind::ConnectionReset
                                    | ErrorKind::BrokenPipe
                                    | ErrorKind::ConnectionRefused
                            ) =>
                        {
                            disc_tx.send(ecs_conn.clone()).unwrap();
                            to_remove.push(connection.id());
                            continue;
                        }
                        _ => (),
                    }
                    let packets = connection.packets_rx.try_iter().collect::<Vec<_>>();
                    for packet in packets {
                        log::trace!("Sending packet {:?}", packet);
                        // If disconnected, the .receive() call above would fail on next loop tick.
                        let _ = connection.send(packet);
                    }
                }
                connections.retain(|(_, ecs_conn)| !to_remove.contains(&ecs_conn.id()));
            }
        });
    }
}

/// A new client has connected.
pub struct NewConnectionEvent<Config: ServerConfig> {
    /// The connection.
    pub connection: ServerConnection<Config>,
    /// A client's IP address.
    pub address: SocketAddr,
}

/// A client disconnected.
pub struct DisconnectionEvent<Config: ServerConfig> {
    /// The connection.
    pub connection: ServerConnection<Config>,
}

/// Sent for every packet received.
pub struct PacketReceiveEvent<Config: ServerConfig> {
    /// The connection.
    pub connection: ServerConnection<Config>,
    /// The packet.
    pub packet: Config::ClientPacket,
}

fn accept_new_connections<Config: ServerConfig>(
    receiver: Res<ConnectionReceiver<Config>>,
    mut event_writer: EventWriter<NewConnectionEvent<Config>>,
) {
    event_writer.send_batch(
        receiver
            .0
            .try_iter()
            .map(|(address, connection)| NewConnectionEvent {
                connection,
                address,
            }),
    )
}

fn accept_new_packets<Config: ServerConfig>(
    receiver: Res<PacketReceiver<Config>>,
    mut event_writer: EventWriter<PacketReceiveEvent<Config>>,
) {
    event_writer.send_batch(
        receiver
            .0
            .try_iter()
            .map(|(connection, packet)| PacketReceiveEvent { connection, packet }),
    )
}

fn remove_connections<Config: ServerConfig>(
    connections: Res<DisconnectionReceiver<Config>>,
    mut writer: EventWriter<DisconnectionEvent<Config>>,
) {
    writer.send_batch(
        connections
            .0
            .try_iter()
            .map(|connection| DisconnectionEvent { connection }),
    )
}

fn connection_add_system<Config: ServerConfig>(
    mut connections: ResMut<Vec<ServerConnection<Config>>>,
    mut events: EventReader<NewConnectionEvent<Config>>,
) {
    for event in events.iter() {
        connections.push(event.connection.clone());
    }
}
