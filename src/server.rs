//! Server part of the plugin. You can enable it by adding `server` feature.

use std::marker::PhantomData;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use bevy::ecs::system::SystemParam;
use bevy::platform::time::Instant;
use bevy::{log, prelude::*};
use tokio::select;
use tokio::sync::mpsc::Receiver;

use crate::connection::{
    max_packet_size_warning_system, set_max_packet_size_system, ConnectionId, DisconnectTask,
    EcsConnection, NetworkQueueSettings, PacketForwarder, RawConnection,
};
use crate::protocol::{Listener, NetworkStream, Protocol, ReadStream, ReceiveError, WriteStream};
use crate::{ServerConfig, SystemSets};

/// Represents the server side of a client connection.
pub type ServerConnection<Config> = EcsConnection<<Config as ServerConfig>::ServerPacket>;
type RawServerConnection<Config> = RawConnection<
    <Config as ServerConfig>::ClientPacket,
    <Config as ServerConfig>::ServerPacket,
    <<Config as ServerConfig>::Protocol as Protocol>::ServerStream,
    <Config as ServerConfig>::EncodeError,
    <Config as ServerConfig>::DecodeError,
    <Config as ServerConfig>::LengthSerializer,
>;

struct ConnectedTransport<Config: ServerConfig> {
    connection: RawServerConnection<Config>,
    ecs_connection: ServerConnection<Config>,
}

/// Tracks client connections registered with this server plugin.
#[derive(Resource)]
pub struct ServerConnections<Config: ServerConfig>(Vec<ServerConnection<Config>>);
impl<Config: ServerConfig> ServerConnections<Config> {
    const fn new() -> Self {
        Self(Vec::new())
    }

    fn register(new_connection: On<NewConnectionEvent<Config>>, mut connections: ResMut<Self>) {
        connections
            .0
            .push(new_connection.event().connection.clone());
    }

    fn remove_connection(&mut self, id: ConnectionId) {
        self.0.retain(|connection| connection.id() != id);
    }
}
impl<Config: ServerConfig> std::ops::Deref for ServerConnections<Config> {
    type Target = Vec<ServerConnection<Config>>;

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
        #[cfg(feature = "protocol_udp")]
        let idle_timeout = crate::protocols::udp::IdleTimeoutSettings::install(app);
        #[cfg(not(feature = "protocol_udp"))]
        let idle_timeout = tokio::sync::watch::channel(std::time::Duration::MAX).1;
        app.insert_resource(ServerConnections::<Config>::new())
            .add_systems(
                Startup,
                (
                    Self::setup_system(self.address, idle_timeout),
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
                    accept_new_packets::<Config>
                        .in_set(SystemSets::ServerAcceptNewPackets)
                        .after(SystemSets::ServerAcceptNewConnections),
                ),
            )
            .add_systems(
                PostUpdate,
                (remove_connections::<Config>.in_set(SystemSets::ServerRemoveConnections),),
            )
            .add_observer(ServerConnections::<Config>::register);
    }
}

impl<Config: ServerConfig> ServerPlugin<Config> {
    /// Configures the endpoint to bind during startup.
    ///
    /// # Panics
    /// Panics if the address cannot be resolved or resolves to no endpoints.
    #[expect(
        clippy::expect_used,
        reason = "Preserve the documented panicking constructor API"
    )]
    pub fn bind<A>(address: A) -> Self
    where
        A: ToSocketAddrs,
    {
        Self {
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
struct ConnectionReceiver<Config: ServerConfig>(Receiver<NewConnectionEvent<Config>>);

#[derive(Resource)]
struct DisconnectionReceiver<Config: ServerConfig>(Receiver<DisconnectionEvent<Config>>);

#[derive(Resource)]
struct PacketReceiver<Config: ServerConfig>(Receiver<PacketReceiveEvent<Config>>);

impl<Config: ServerConfig> ServerPlugin<Config> {
    #[expect(
        clippy::too_many_lines,
        reason = "Keep the connection task lifecycle and its channel wiring together"
    )]
    fn setup_system(
        address: SocketAddr,
        idle_timeout: tokio::sync::watch::Receiver<std::time::Duration>,
    ) -> impl Fn(Commands, Option<Res<NetworkQueueSettings>>) {
        #[cfg(target_family = "wasm")]
        compile_error!("Why would you run a bevy_slinet server on WASM? If you really need this, please open an issue (https://github.com/aggyomfg/bevy_slinet/issues/new)");

        move |mut commands: Commands, queues: Option<Res<NetworkQueueSettings>>| {
            let queues = queues.as_deref().copied().unwrap_or_default();
            let (conn_tx, conn_rx) = queues.incoming_channel();
            let (connection_sender, mut incoming_connections) =
                queues.incoming_channel::<ConnectedTransport<Config>>();
            let (disc_tx, disc_rx) = queues.incoming_channel();
            let (pack_tx, pack_rx) = queues.incoming_channel();
            let (disconnect_sender, mut incoming_disconnects) = queues.incoming_channel();
            commands.insert_resource(ConnectionReceiver::<Config>(conn_rx));
            commands.insert_resource(DisconnectionReceiver::<Config>(disc_rx));
            commands.insert_resource(PacketReceiver::<Config>(pack_rx));
            let (bound_tx, bound_rx) = std::sync::mpsc::sync_channel::<SocketAddr>(1);
            let idle_timeout = idle_timeout.clone();

            std::thread::spawn(move || {
                let runtime_result = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build();

                let runtime = match runtime_result {
                    Ok(rt) => rt,
                    Err(err) => {
                        log::error!("Failed to create tokio runtime: {}", err);
                        return;
                    }
                };

                runtime.block_on(async move {
                tokio::spawn(async move {
                    while let Some(ConnectedTransport { connection, ecs_connection: ecs_conn }) = incoming_connections.recv().await {
                        let RawConnection {
                            disconnect_task,
                            stream,
                            serializer,
                            packet_length_serializer,
                            mut packets_rx,
                            id,
                        } = connection;
                        let (mut read, mut write) = match stream.into_split().await {
                            Ok(split) => split,
                            Err(err) => {
                                log::error!("({:?}) Couldn't split stream: {}", id, err);
                                continue;
                            }
                        };
                        let pack_tx2 = pack_tx.clone();
                        let disconnect_forwarder = disc_tx.clone();
                        let serializer2 = Arc::clone(&serializer);
                        let disc_tx2_2 = disconnect_sender.clone();
                        let packet_length_serializer2 = Arc::clone(&packet_length_serializer);
                        read.set_idle_timeout(idle_timeout.clone());
                        let write_cancel = disconnect_task.clone();
                        tokio::spawn(async move {
                            let _guard = disconnect_task.clone().drop_guard();
                            let packets = PacketForwarder::new(
                                pack_tx2,
                                Config::Protocol::DATAGRAM,
                                disconnect_task.clone(),
                            );
                            let error = loop {
                                tokio::select! {
                                    biased;
                                    () = disconnect_task.cancelled() => break ReceiveError::IntentionalDisconnection,
                                    result = read.receive_with_timestamp(Arc::clone(&serializer2), &*packet_length_serializer2) => {
                                        match result {
                                            Ok((packet, received_at)) => {
                                                log::trace!("({id:?}) Received packet {packet:?}");
                                                if !packets.forward(PacketReceiveEvent::<Config> {
                                                    connection: ecs_conn.clone(),
                                                    packet,
                                                    received_at,
                                                }).await {
                                                    break ReceiveError::IntentionalDisconnection;
                                                }
                                            }
                                            Err(err) => break err,
                                        }
                                    }
                                }
                            };
                            disconnect_task.cancel();
                            read.close();
                            if let Err(err) = disconnect_forwarder.send(DisconnectionEvent::<Config> {
                                error,
                                connection: ecs_conn.clone(),
                            }).await {
                                log::debug!("({id:?}) Disconnection receiver closed: {err:?}");
                            }
                            if let Err(err) = disc_tx2_2.send(ecs_conn.peer_addr).await {
                                log::debug!("({id:?}) Listener closed: {err}");
                            }
                        });
                        tokio::spawn(async move {
                            let _guard = write_cancel.clone().drop_guard();
                            let sending = async {
                                while let Some(packet) = packets_rx.recv().await {
                                        if write_cancel.is_cancelled() { break; }
                                    log::trace!("({id:?}) Sending packet {packet:?}");
                                    if let Err(err) = write.send(packet, Arc::clone(&serializer), &*packet_length_serializer).await {
                                        log::error!("({id:?}) Error sending packet: {err}");
                                        break;
                                    }
                                }
                            };
                            tokio::select! {
                                biased;
                                () = write_cancel.cancelled() => {},
                                () = sending => {},
                            }
                        });
                    }
                });

                let binding_result = Config::Protocol::bind(address).await;
                let listener = match binding_result {
                    Ok(listener) => listener,
                    Err(err) => {
                        log::error!("Couldn't create listener at {}: {}", address, err);
                        return;
                    }
                };
                let _ = bound_tx.send(listener.address());

                let mut warned = false;
                loop {
                    select! {
                        Ok(connection) = listener.accept() => {
                            log::debug!("Accepting a connection from {:?}", connection.peer_addr());
                            let connection_forwarder = conn_tx.clone();
                            let conn_tx2_2 = connection_sender.clone();
                            let serializer = Config::build_serializer();
                            serializer.warn_if_stateful_over_datagrams::<Config::Protocol>(&mut warned);
                            tokio::spawn(async move {
                                let (tx, rx) = queues.outgoing_channel();
                                let disconnect_task = DisconnectTask::default();
                                let connection = RawConnection {
                                    disconnect_task: disconnect_task.clone(),
                                    stream: connection,
                                    serializer: Arc::new(serializer),
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
                                if let Err(err) = connection_forwarder.send(NewConnectionEvent::<Config> {
                                    address: ecs_conn.peer_addr,
                                    connection: ecs_conn.clone(),
                                }).await {
                                    log::error!("Failed to send new connection to ECS: {}", err);
                                    return;
                                }
                                if let Err(err) = conn_tx2_2.send(ConnectedTransport::<Config> {
                                    connection,
                                    ecs_connection: ecs_conn,
                                }).await {
                                    log::error!("Failed to send new raw connection: {}", err);
                                }
                            });
                        }
                        Some(addr) = incoming_disconnects.recv() => {
                            listener.handle_disconnection(addr);
                        }
                        else => {
                            break;
                        }
                    }
                }
            });
            });

            // Clients may connect right after Startup, so the listener must exist by then.
            if let Ok(local_addr) = bound_rx.recv() {
                commands.insert_resource(ServerAddress::<Config> {
                    address: local_addr,
                    _marker: PhantomData,
                });
            }
        }
    }
}

/// The address the server is actually listening on, available after `Startup`.
/// Not inserted if binding failed. Useful when binding to port `0`.
#[derive(Resource)]
pub struct ServerAddress<Config: ServerConfig> {
    address: SocketAddr,
    _marker: PhantomData<Config>,
}

impl<Config: ServerConfig> ServerAddress<Config> {
    /// The bound address.
    #[must_use]
    pub const fn address(&self) -> SocketAddr {
        self.address
    }
}

/// A new client has connected.
#[derive(Event)]
#[non_exhaustive]
pub struct NewConnectionEvent<Config: ServerConfig> {
    /// The connection.
    pub connection: ServerConnection<Config>,
    /// A client's IP address.
    pub address: SocketAddr,
}

/// A client disconnected.
#[derive(Event)]
#[non_exhaustive]
pub struct DisconnectionEvent<Config: ServerConfig> {
    /// The error.
    pub error: ReceiveError<Config::DecodeError, Config::LengthSerializer>,
    /// The connection.
    pub connection: ServerConnection<Config>,
}

/// Sent for every packet received.
#[derive(Event)]
#[non_exhaustive]
pub struct PacketReceiveEvent<Config: ServerConfig> {
    /// The connection.
    pub connection: ServerConnection<Config>,
    /// The packet.
    pub packet: Config::ClientPacket,
    /// When the built-in transport finished reading the packet, before decoding or queueing.
    /// Custom protocols use [`ReadStream::receive_with_timestamp`] semantics.
    pub received_at: Instant,
}

/// Accepted connections and the frame budget governing their delivery to observers.
#[derive(SystemParam)]
struct IncomingConnections<'w, Config: ServerConfig> {
    connections: ResMut<'w, ConnectionReceiver<Config>>,
    queues: Option<Res<'w, NetworkQueueSettings>>,
}

impl<Config: ServerConfig> IncomingConnections<'_, Config> {
    fn drain(&mut self) -> impl Iterator<Item = NewConnectionEvent<Config>> + '_ {
        let limit = self
            .queues
            .as_deref()
            .copied()
            .unwrap_or_default()
            .events_per_frame;
        let receiver = &mut self.connections.0;
        std::iter::from_fn(move || receiver.try_recv().ok()).take(limit)
    }
}

fn accept_new_connections<Config: ServerConfig>(
    mut incoming: IncomingConnections<Config>,
    mut commands: Commands,
) {
    for connection in incoming.drain() {
        commands.trigger(connection);
    }
}

/// Received packets and the frame budget governing their delivery to observers.
#[derive(SystemParam)]
struct IncomingPackets<'w, Config: ServerConfig> {
    packets: ResMut<'w, PacketReceiver<Config>>,
    queues: Option<Res<'w, NetworkQueueSettings>>,
}

impl<Config: ServerConfig> IncomingPackets<'_, Config> {
    fn drain(&mut self) -> impl Iterator<Item = PacketReceiveEvent<Config>> + '_ {
        let limit = self
            .queues
            .as_deref()
            .copied()
            .unwrap_or_default()
            .events_per_frame;
        let receiver = &mut self.packets.0;
        std::iter::from_fn(move || receiver.try_recv().ok()).take(limit)
    }
}

fn accept_new_packets<Config: ServerConfig>(
    mut incoming: IncomingPackets<Config>,
    mut commands: Commands,
) {
    for packet in incoming.drain() {
        commands.trigger(packet);
    }
}

/// Closed connections and the frame budget for removing them from the registry.
#[derive(SystemParam)]
struct IncomingDisconnections<'w, Config: ServerConfig> {
    disconnections: ResMut<'w, DisconnectionReceiver<Config>>,
    queues: Option<Res<'w, NetworkQueueSettings>>,
}

impl<Config: ServerConfig> IncomingDisconnections<'_, Config> {
    fn drain(&mut self) -> impl Iterator<Item = DisconnectionEvent<Config>> + '_ {
        let limit = self
            .queues
            .as_deref()
            .copied()
            .unwrap_or_default()
            .events_per_frame;
        let receiver = &mut self.disconnections.0;
        std::iter::from_fn(move || receiver.try_recv().ok()).take(limit)
    }
}

fn remove_connections<Config: ServerConfig>(
    mut connections: ResMut<ServerConnections<Config>>,
    mut incoming: IncomingDisconnections<Config>,
    mut commands: Commands,
) {
    for event in incoming.drain() {
        connections.remove_connection(event.connection.id());
        commands.trigger(event);
    }
}
