//! Client part of the plugin. You can enable it by adding `client` feature.

use std::io;
use std::io::ErrorKind;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::sync::atomic::Ordering;

use bevy::prelude::*;
use crossbeam_channel::{Receiver, Sender};

use crate::connection::{
    max_packet_size_system, EcsConnection, RawConnection, ReceiveError, MAX_PACKET_SIZE,
};
use crate::protocol::Protocol;
use crate::{ClientConfig, SystemLabels};

/// Client-side connection to a server.
pub type ClientConnection<Config> = EcsConnection<<Config as ClientConfig>::ClientPacket>;

/// Client-side plugin. Use [`ClientPlugin::connect`] to connect immediately or
/// [`ClientPlugin::new`] to add the required systems and send [`ConnectionRequestEvent`] later.
pub struct ClientPlugin<Config: ClientConfig> {
    address: Option<SocketAddr>,
    _marker: PhantomData<Config>,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, SystemLabel)]
struct AddInitialConnectionRequestEventLabel;

impl<Config: ClientConfig> Plugin for ClientPlugin<Config> {
    fn build(&self, app: &mut App) {
        if MAX_PACKET_SIZE.load(Ordering::Relaxed) == usize::MAX {
            log::warn!("You haven't set \"MaxPacketSize\" resource! This is a security risk, please insert it before using this client in production.")
        }

        let address = self.address;

        app.add_event::<ConnectionRequestEvent<Config>>()
            .add_event::<ConnectionEstablishEvent<Config>>()
            .add_event::<DisconnectionEvent<Config>>()
            .add_event::<PacketReceiveEvent<Config>>()
            .add_system(max_packet_size_system.label(SystemLabels::SetMaxPacketSize))
            .add_system(
                connection_request_system::<Config>.label(SystemLabels::ClientConnectionRequest),
            )
            .add_system(
                connection_establish_system::<Config>
                    .label(SystemLabels::ClientConnectionEstablish),
            )
            .add_system(
                connection_remove_system::<Config>.label(SystemLabels::ClientConnectionRemove),
            )
            .add_system(packet_receive_system::<Config>.label(SystemLabels::ClientPacketReceive))
            .add_startup_system(
                (move |mut events: EventWriter<ConnectionRequestEvent<Config>>| {
                    if let Some(address) = address {
                        events.send(ConnectionRequestEvent::new(address));
                    }
                })
                .label(AddInitialConnectionRequestEventLabel),
            )
            .add_startup_system(
                setup_system::<Config>.after(AddInitialConnectionRequestEventLabel),
            );
    }
}

impl<Config: ClientConfig> Default for ClientPlugin<Config> {
    fn default() -> Self {
        ClientPlugin {
            address: None,
            _marker: PhantomData,
        }
    }
}

impl<Config: ClientConfig> ClientPlugin<Config> {
    /// Adds the required systems, but doesn't connect immediately.
    /// Create a [ClientConnection](ClientConnection) using [`Protocol::connect_to_server`]
    /// and add it as a resource to start the server.
    pub fn new() -> ClientPlugin<Config> {
        ClientPlugin::default()
    }

    /// Adds the required systems and connects immediately.
    pub fn connect<A>(addr: A) -> ClientPlugin<Config>
    where
        A: ToSocketAddrs,
    {
        ClientPlugin {
            address: Some(
                addr.to_socket_addrs()
                    .expect("Invalid address")
                    .next()
                    .expect("Invalid address"),
            ),
            _marker: PhantomData,
        }
    }
}

/// Send this event to indicate that you want to connect to a server.
/// Wait for [`ConnectionEstablishEvent`] or [`DisconnectionEvent`] to know the connection's state
pub struct ConnectionRequestEvent<Config: ClientConfig> {
    address: SocketAddr,
    _marker: PhantomData<Config>,
}

impl<Config: ClientConfig> ConnectionRequestEvent<Config> {
    /// Create a new connection request.
    pub fn new(address: impl ToSocketAddrs) -> ConnectionRequestEvent<Config> {
        ConnectionRequestEvent {
            address: address
                .to_socket_addrs()
                .expect("Invalid address")
                .next()
                .expect("Invalid address"),
            _marker: PhantomData,
        }
    }
}

impl<Config: ClientConfig> Clone for ConnectionRequestEvent<Config> {
    fn clone(&self) -> Self {
        ConnectionRequestEvent::new(&self.address)
    }
}

struct ConnectionRequestSender<Config: ClientConfig>(Sender<SocketAddr>, PhantomData<Config>);
struct ConnectionReceiver<Config: ClientConfig>(Receiver<(SocketAddr, ClientConnection<Config>)>);
struct DisconnectionReceiver<Config: ClientConfig>(
    Receiver<(io::Error, SocketAddr)>,
    PhantomData<Config>,
);
struct PacketReceiver<Config: ClientConfig>(
    Receiver<(ClientConnection<Config>, Config::ServerPacket)>,
);

fn setup_system<Config: ClientConfig>(mut commands: Commands) {
    let (req_tx, req_rx) = crossbeam_channel::unbounded();
    commands.insert_resource(ConnectionRequestSender::<Config>(req_tx, PhantomData));

    let (conn_tx, conn_rx) = crossbeam_channel::unbounded();
    let (conn_tx2, conn_rx2) = crossbeam_channel::unbounded();
    let (disc_tx, disc_rx) = crossbeam_channel::unbounded();
    let (pack_tx, pack_rx) = crossbeam_channel::unbounded();
    commands.insert_resource(ConnectionReceiver::<Config>(conn_rx));
    commands.insert_resource(DisconnectionReceiver::<Config>(disc_rx, PhantomData));
    commands.insert_resource(PacketReceiver::<Config>(pack_rx));

    // Connection
    let disc_tx_2 = disc_tx.clone();
    std::thread::spawn(move || {
        for address in req_rx.iter() {
            let (tx, rx) = crossbeam_channel::unbounded();
            match create_connection::<Config>(
                address,
                Config::Serializer::default(),
                Config::LengthSerializer::default(),
                rx,
            ) {
                Ok(connection) => {
                    let ecs_conn = EcsConnection {
                        id: connection.id(),
                        packet_tx: tx,
                        local_addr: connection.local_addr(),
                        peer_addr: connection.peer_addr(),
                    };
                    conn_tx.send((address, ecs_conn.clone())).unwrap();
                    conn_tx2.send((connection, ecs_conn)).unwrap();
                }
                Err(err) => {
                    log::warn!("Couldn't connect to server: {err:?}");
                    disc_tx_2.send((err, address)).unwrap();
                }
            }
        }

        io::Result::Ok(())
    });

    std::thread::spawn(move || {
        let mut connections = Vec::new();

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
                        disc_tx.send((err, connection.peer_addr())).unwrap();
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
            connections.retain(|(connection, _)| !to_remove.contains(&connection.id()));
        }
    });
}

fn connection_request_system<Config: ClientConfig>(
    requests: Res<ConnectionRequestSender<Config>>,
    mut events: EventReader<ConnectionRequestEvent<Config>>,
) {
    for event in events.iter() {
        requests.0.send(event.address).unwrap();
    }
}

#[allow(clippy::type_complexity)]
pub(crate) fn create_connection<Config: ClientConfig>(
    addr: SocketAddr,
    serializer: Config::Serializer,
    packet_length_serializer: Config::LengthSerializer,
    packet_rx: Receiver<Config::ClientPacket>,
) -> io::Result<
    RawConnection<
        Config::ServerPacket,
        Config::ClientPacket,
        <Config::Protocol as Protocol>::ClientStream,
        Config::Serializer,
        Config::LengthSerializer,
    >,
> {
    Ok(RawConnection::new(
        Config::Protocol::connect_to_server(addr)?,
        serializer,
        packet_length_serializer,
        packet_rx,
    ))
}

fn packet_receive_system<Config: ClientConfig>(
    packets: Res<PacketReceiver<Config>>,
    mut writer: EventWriter<PacketReceiveEvent<Config>>,
) {
    writer.send_batch(
        packets
            .0
            .try_iter()
            .map(|(connection, packet)| PacketReceiveEvent { connection, packet }),
    )
}

fn connection_establish_system<Config: ClientConfig>(
    mut commands: Commands,
    new_connections: Res<ConnectionReceiver<Config>>,
    mut connections: Option<ResMut<Vec<ClientConnection<Config>>>>,
    mut writer: EventWriter<ConnectionEstablishEvent<Config>>,
) {
    for (address, connection) in new_connections.0.try_iter() {
        commands.insert_resource(connection.clone());
        if let Some(connections) = connections.as_mut() {
            connections.push(connection.clone());
        } else {
            commands.insert_resource(vec![connection.clone()]);
        };
        writer.send(ConnectionEstablishEvent {
            address,
            connection,
        })
    }
}

fn connection_remove_system<Config: ClientConfig>(
    mut commands: Commands,
    old_connections: Res<DisconnectionReceiver<Config>>,
    mut connections: Option<ResMut<Vec<ClientConnection<Config>>>>,
    mut writer: EventWriter<DisconnectionEvent<Config>>,
) {
    for (error, address) in old_connections.0.try_iter() {
        commands.remove_resource::<ClientConnection<Config>>();
        if let Some(connections) = connections.as_mut() {
            connections.retain(|conn| conn.peer_addr() != address);
        }
        writer.send(DisconnectionEvent {
            error,
            address,
            _marker: PhantomData,
        })
    }
}

/// Indicates that a connection was successfully established.
pub struct ConnectionEstablishEvent<Config: ClientConfig> {
    /// A server address.
    pub address: SocketAddr,
    /// The connection.
    pub connection: ClientConnection<Config>,
}

/// Indicates that something went wrong during a connection attempt. See [`DisconnectionEvent.error`] for details
pub struct DisconnectionEvent<Config: ClientConfig> {
    /// The error.
    pub error: io::Error,
    /// A server's IP address.
    pub address: SocketAddr,
    _marker: PhantomData<Config>,
}

/// Sent for every packet received.
pub struct PacketReceiveEvent<Config: ClientConfig> {
    /// The connection.
    pub connection: ClientConnection<Config>,
    /// The packet.
    pub packet: Config::ServerPacket,
}
