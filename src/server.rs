//! Server part of the plugin. You can enable it by adding `server` feature.

use std::marker::PhantomData;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use bevy::prelude::*;
use tokio::select;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::connection::{
    max_packet_size_warning_system, set_max_packet_size_system, ConnectionId, DisconnectTask,
    EcsConnection, RawConnection,
};
use crate::protocol::{Listener, NetworkStream, Protocol, ReadStream, ReceiveError, WriteStream};
use crate::{ServerConfig, SystemSets};

/// Server-side connection to a server.
pub type EcsServerConnection<Config> = EcsConnection<<Config as ServerConfig>::ServerPacket>;
type RawServerConnection<Config> = (
    RawConnection<
        <Config as ServerConfig>::ClientPacket,
        <Config as ServerConfig>::ServerPacket,
        <<Config as ServerConfig>::Protocol as Protocol>::ServerStream,
        <Config as ServerConfig>::SerializerError,
        <Config as ServerConfig>::LengthSerializer,
    >,
    EcsServerConnection<Config>,
);
/// List of server-side connections to a server.
#[derive(Resource)]
pub struct ServerConnections<Config: ServerConfig>(Vec<EcsServerConnection<Config>>);
impl<Config: ServerConfig> ServerConnections<Config> {
    fn new() -> Self {
        Self(Vec::new())
    }
}
impl<Config: ServerConfig> std::ops::Deref for ServerConnections<Config> {
    type Target = Vec<EcsServerConnection<Config>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl<Config: ServerConfig> std::ops::DerefMut for ServerConnections<Config> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Server-side plugin. Use [`ServerPlugin::bind`] to create.
pub struct ServerPlugin<Config: ServerConfig> {
    address: SocketAddr,
    _marker: PhantomData<Config>,
}

impl<Config: ServerConfig> Plugin for ServerPlugin<Config> {
    fn build(&self, app: &mut App) {
        app.add_event::<NewConnectionEvent<Config>>()
            .add_event::<DisconnectionEvent<Config>>()
            .add_event::<PacketReceiveEvent<Config>>()
            .insert_resource(ServerConnections::<Config>::new())
            .add_systems(
                Startup,
                (
                    create_setup_system::<Config>(self.address),
                    max_packet_size_warning_system.in_set(SystemSets::MaxPacketSizeWarning),
                ),
            )
            .add_systems(
                Update,
                set_max_packet_size_system.in_set(SystemSets::SetMaxPacketSize),
            )
            .add_systems(
                PreUpdate,
                (
                    accept_new_connections::<Config>.in_set(SystemSets::ServerAcceptNewConnections),
                    accept_new_packets::<Config>.in_set(SystemSets::ServerAcceptNewPackets),
                ),
            )
            .add_systems(
                PostUpdate,
                (
                    remove_connections::<Config>.in_set(SystemSets::ServerRemoveConnections),
                    connection_add_system::<Config>.in_set(SystemSets::ServerConnectionAdd),
                ),
            );
    }
}

impl<Config: ServerConfig> ServerPlugin<Config> {
    /// Bind to the specified address and return a [`ServerPlugin`].
    pub fn bind<A>(address: A) -> ServerPlugin<Config>
    where
        A: ToSocketAddrs,
    {
        ServerPlugin {
            address: address
                .to_socket_addrs()
                .expect("Invalid address")
                .next()
                .expect("Invalid address"),
            _marker: PhantomData,
        }
    }
}

#[derive(Resource)]
struct ConnectionReceiver<Config: ServerConfig>(
    UnboundedReceiver<(SocketAddr, EcsServerConnection<Config>)>,
);

#[allow(clippy::type_complexity)]
#[derive(Resource)]
struct DisconnectionReceiver<Config: ServerConfig>(
    UnboundedReceiver<(
        ReceiveError<Config::SerializerError, Config::LengthSerializer>,
        EcsServerConnection<Config>,
    )>,
);

#[derive(Resource)]
struct PacketReceiver<Config: ServerConfig>(
    UnboundedReceiver<(EcsServerConnection<Config>, Config::ClientPacket)>,
);

fn create_setup_system<Config: ServerConfig>(address: SocketAddr) -> impl Fn(Commands) {
    #[cfg(target_family = "wasm")]
    compile_error!("Why would you run a bevy_slinet server on WASM? If you really need this, please open an issue (https://github.com/Sliman4/bevy_slinet/issues/new)");

    move |mut commands: Commands| {
        let (conn_tx, conn_rx) = tokio::sync::mpsc::unbounded_channel();
        let (conn_tx2, mut conn_rx2): (
            UnboundedSender<RawServerConnection<Config>>,
            UnboundedReceiver<RawServerConnection<Config>>,
        ) = tokio::sync::mpsc::unbounded_channel();
        let (disc_tx, disc_rx) = tokio::sync::mpsc::unbounded_channel();
        let (pack_tx, pack_rx) = tokio::sync::mpsc::unbounded_channel();
        let (disc_tx2, mut disc_rx2) = tokio::sync::mpsc::unbounded_channel();
        commands.insert_resource(ConnectionReceiver::<Config>(conn_rx));
        commands.insert_resource(DisconnectionReceiver::<Config>(disc_rx));
        commands.insert_resource(PacketReceiver::<Config>(pack_rx));

        std::thread::spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("Cannot start tokio runtime")
                .block_on(async move {
                    // Receiving packets
                    tokio::spawn(async move {
                        while let Some((connection, ecs_conn)) = conn_rx2.recv().await {
                            let RawConnection {
                                disconnect_task,
                                stream,
                                serializer,
                                packet_length_serializer,
                                mut packets_rx,
                                id,
                            } = connection;
                            let (mut read, mut write) =
                                stream.into_split().await.expect("Couldn't split stream");
                            let pack_tx2 = pack_tx.clone();
                            let disc_tx_2 = disc_tx.clone();
                            let serializer2 = Arc::clone(&serializer);
                            let disc_tx2_2 = disc_tx2.clone();
                            let packet_length_serializer2 = Arc::clone(&packet_length_serializer);
                            tokio::spawn(async move {
                                loop {
                                    // `select!` handles intentional disconnections (ecs_connection.disconnect()).
                                    // AsyncReadExt::read_exact is not cancel-safe and loses data, but we don't need that data anymore
                                    tokio::select! {
                                        result = read.receive(Arc::clone(&serializer2), &*packet_length_serializer2) => {
                                            match result {
                                                Ok(packet) => {
                                                    log::trace!("({id:?}) Received packet {:?}", packet);
                                                    pack_tx2.send((ecs_conn.clone(), packet)).unwrap();
                                                }
                                                Err(err) => {
                                                    disc_tx_2.send((err, ecs_conn.clone())).unwrap();
                                                    disc_tx2_2.send(ecs_conn.peer_addr).unwrap();
                                                    break;
                                                }
                                            }
                                        }
                                        _ = disconnect_task.clone() => {
                                            log::debug!("({id:?}) Client was disconnected intentionally");
                                            disc_tx_2.send((ReceiveError::IntentionalDisconnection, ecs_conn.clone())).unwrap();
                                            disc_tx2_2.send(ecs_conn.peer_addr).unwrap();
                                            break;
                                        }
                                    };
                                }
                            });
                            // `select!` is not needed because `packets_rx` returns `None` when
                            // all senders are be dropped, and `disc_tx2.send(...)` above should
                            // remove all senders from ECS.
                            tokio::spawn(async move {
                                while let Some(packet) = packets_rx.recv().await {
                                    log::trace!("({id:?}) Sending packet {:?}", packet);
                                    match write
                                        .send(packet, Arc::clone(&serializer), &*packet_length_serializer)
                                        .await
                                    {
                                        Ok(()) => (),
                                        Err(err) => {
                                            log::error!("({id:?}) Error sending packet: {err}");
                                            break;
                                        }
                                    }
                                }
                            });
                        }
                    });

                    // New connections
                    let listener = Config::Protocol::bind(address)
                        .await
                        .expect("Couldn't create listener");

                    loop {
                        select! {
                            Ok(connection) = listener.accept() => {
                                log::debug!("Accepting a connection from {:?}", connection.peer_addr());
                                let (conn_tx_2, conn_tx2_2) = (conn_tx.clone(), conn_tx2.clone());
                                tokio::spawn(async move {
                                    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                                    let disconnect_task = DisconnectTask::default();
                                    let connection = RawConnection {
                                        disconnect_task: disconnect_task.clone(),
                                        stream: connection,
                                        serializer: Arc::new(Config::build_serializer()),
                                        packet_length_serializer: Arc::new(Default::default()),
                                        id: ConnectionId::next(),
                                        packets_rx: rx,
                                    };
                                    let ecs_conn = EcsConnection {
                                        disconnect_task,
                                        id: connection.id(),
                                        packet_tx: tx,
                                        local_addr: connection.local_addr(),
                                        peer_addr: connection.peer_addr(),
                                    };
                                    conn_tx_2.send((address, ecs_conn.clone())).unwrap();
                                    conn_tx2_2.send((connection, ecs_conn)).unwrap();
                                });
                            }
                            Some(addr) = disc_rx2.recv() => {
                                listener.handle_disconnection(addr);
                            }
                            else => {
                                break;
                            }
                        }
                    }
                });
        });
    }
}

/// A new client has connected.
#[derive(Event)]
pub struct NewConnectionEvent<Config: ServerConfig> {
    /// The connection.
    pub connection: EcsServerConnection<Config>,
    /// A client's IP address.
    pub address: SocketAddr,
}

/// A client disconnected.
#[derive(Event)]
pub struct DisconnectionEvent<Config: ServerConfig> {
    /// The error.
    pub error: ReceiveError<Config::SerializerError, Config::LengthSerializer>,
    /// The connection.
    pub connection: EcsServerConnection<Config>,
}

/// Sent for every packet received.
#[derive(Event)]
pub struct PacketReceiveEvent<Config: ServerConfig> {
    /// The connection.
    pub connection: EcsServerConnection<Config>,
    /// The packet.
    pub packet: Config::ClientPacket,
}

fn accept_new_connections<Config: ServerConfig>(
    mut receiver: ResMut<ConnectionReceiver<Config>>,
    mut event_writer: EventWriter<NewConnectionEvent<Config>>,
) {
    while let Ok((address, connection)) = receiver.0.try_recv() {
        let _id = event_writer.send(NewConnectionEvent {
            connection,
            address,
        });
    }
}

fn accept_new_packets<Config: ServerConfig>(
    mut receiver: ResMut<PacketReceiver<Config>>,
    mut event_writer: EventWriter<PacketReceiveEvent<Config>>,
) {
    while let Ok((connection, packet)) = receiver.0.try_recv() {
        let _id = event_writer.send(PacketReceiveEvent { connection, packet });
    }
}

fn remove_connections<Config: ServerConfig>(
    mut connections: ResMut<ServerConnections<Config>>,
    mut disconnections: ResMut<DisconnectionReceiver<Config>>,
    mut event_writer: EventWriter<DisconnectionEvent<Config>>,
) {
    while let Ok((error, connection)) = disconnections.0.try_recv() {
        connections.retain(|conn| conn.id() != connection.id());
        event_writer.send(DisconnectionEvent { error, connection });
    }
}

fn connection_add_system<Config: ServerConfig>(
    mut connections: ResMut<ServerConnections<Config>>,
    mut events: EventReader<NewConnectionEvent<Config>>,
) {
    for event in events.read() {
        connections.push(event.connection.clone());
    }
}
