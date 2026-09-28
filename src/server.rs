//! Server part of the plugin. You can enable it by adding `server` feature.

use std::future::Future;
use std::marker::PhantomData;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use bevy::ecs::system::SystemParam;
use bevy::platform::time::Instant;
use bevy::{log, prelude::*};
use tokio::select;
use tokio::sync::mpsc::{Receiver, Sender};

use crate::connection::{
    ConnectionId, DisconnectTask, EcsConnection, MaxPacketSize, NetworkQueueSettings,
    PacketForwarder, RawConnection,
};
use crate::protocols::protocol::{
    Listener, NetworkStream, Protocol, ReadStream, ReceiveError, WriteStream,
};
use crate::serializers::serializer::Serializer;
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

type ServerSerializer<Config> = dyn Serializer<
    <Config as ServerConfig>::ClientPacket,
    <Config as ServerConfig>::ServerPacket,
    EncodeError = <Config as ServerConfig>::EncodeError,
    DecodeError = <Config as ServerConfig>::DecodeError,
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
                    MaxPacketSize::warning_system.in_set(SystemSets::MaxPacketSizeWarning),
                ),
            )
            .add_systems(
                Update,
                MaxPacketSize::set_system.in_set(SystemSets::SetMaxPacketSize),
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
    fn setup_system(
        address: SocketAddr,
        idle_timeout: tokio::sync::watch::Receiver<std::time::Duration>,
    ) -> impl Fn(Commands, Option<Res<NetworkQueueSettings>>) {
        #[cfg(target_family = "wasm")]
        compile_error!("Why would you run a bevy_slinet server on WASM? If you really need this, please open an issue (https://github.com/aggyomfg/bevy_slinet/issues/new)");

        move |commands, queues| {
            Self::setup(
                commands,
                address,
                idle_timeout.clone(),
                queues.as_deref().copied().unwrap_or_default(),
            );
        }
    }

    fn setup(
        mut commands: Commands,
        address: SocketAddr,
        idle_timeout: tokio::sync::watch::Receiver<std::time::Duration>,
        queues: NetworkQueueSettings,
    ) {
        let (conn_tx, conn_rx) = queues.incoming_channel();
        let (connection_sender, incoming_connections) = queues.incoming_channel();
        let (disc_tx, disc_rx) = queues.incoming_channel();
        let (pack_tx, pack_rx) = queues.incoming_channel();
        let (disconnect_sender, incoming_disconnects) = queues.incoming_channel();
        commands.insert_resource(ConnectionReceiver::<Config>(conn_rx));
        commands.insert_resource(DisconnectionReceiver::<Config>(disc_rx));
        commands.insert_resource(PacketReceiver::<Config>(pack_rx));
        let (bound_tx, bound_rx) = std::sync::mpsc::sync_channel(1);

        Self::run_async(move || async move {
            tokio::spawn(Self::process_connections(
                incoming_connections,
                pack_tx,
                disc_tx,
                disconnect_sender,
                idle_timeout,
            ));
            Self::accept_connections(
                address,
                queues,
                conn_tx,
                connection_sender,
                incoming_disconnects,
                bound_tx,
            )
            .await;
        });

        // Clients may connect right after Startup, so the listener must exist by then.
        if let Ok(local_addr) = bound_rx.recv() {
            commands.insert_resource(ServerAddress::<Config> {
                address: local_addr,
                _marker: PhantomData,
            });
        }
    }

    // Build the future on its runtime thread: custom listeners need not be Send.
    fn run_async<F>(make_future: impl FnOnce() -> F + Send + 'static)
    where
        F: Future<Output = ()>,
    {
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    log::error!("Failed to create tokio runtime: {}", err);
                    return;
                }
            };
            runtime.block_on(make_future());
        });
    }

    async fn accept_connections(
        address: SocketAddr,
        queues: NetworkQueueSettings,
        connections: Sender<NewConnectionEvent<Config>>,
        connection_sender: Sender<ConnectedTransport<Config>>,
        mut incoming_disconnects: Receiver<SocketAddr>,
        bound_tx: std::sync::mpsc::SyncSender<SocketAddr>,
    ) {
        let listener = match Config::Protocol::bind(address).await {
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
                Ok(stream) = listener.accept() => {
                    log::debug!("Accepting a connection from {:?}", stream.peer_addr());
                    let serializer = Config::build_serializer();
                    serializer.warn_if_stateful_over_datagrams::<Config::Protocol>(&mut warned);
                    tokio::spawn(ConnectedTransport::<Config>::establish(
                        stream,
                        Arc::new(serializer),
                        queues,
                        connections.clone(),
                        connection_sender.clone(),
                    ));
                }
                Some(addr) = incoming_disconnects.recv() => {
                    listener.handle_disconnection(addr);
                }
                else => break,
            }
        }
    }

    async fn process_connections(
        mut incoming_connections: Receiver<ConnectedTransport<Config>>,
        packets: Sender<PacketReceiveEvent<Config>>,
        disconnections: Sender<DisconnectionEvent<Config>>,
        disconnect_sender: Sender<SocketAddr>,
        idle_timeout: tokio::sync::watch::Receiver<std::time::Duration>,
    ) {
        while let Some(connection) = incoming_connections.recv().await {
            connection
                .run(
                    packets.clone(),
                    disconnections.clone(),
                    disconnect_sender.clone(),
                    idle_timeout.clone(),
                )
                .await;
        }
    }
}

impl<Config: ServerConfig> ConnectedTransport<Config> {
    async fn establish(
        stream: <Config::Protocol as Protocol>::ServerStream,
        serializer: Arc<ServerSerializer<Config>>,
        queues: NetworkQueueSettings,
        connections: Sender<NewConnectionEvent<Config>>,
        connection_sender: Sender<Self>,
    ) {
        let (tx, rx) = queues.outgoing_channel();
        let disconnect_task = DisconnectTask::default();
        let connection = RawConnection {
            disconnect_task: disconnect_task.clone(),
            stream,
            serializer,
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
        if let Err(err) = connections
            .send(NewConnectionEvent::<Config> {
                address: ecs_conn.peer_addr,
                connection: ecs_conn.clone(),
            })
            .await
        {
            log::error!("Failed to send new connection to ECS: {}", err);
            return;
        }
        if let Err(err) = connection_sender
            .send(Self {
                connection,
                ecs_connection: ecs_conn,
            })
            .await
        {
            log::error!("Failed to send new raw connection: {}", err);
        }
    }

    async fn run(
        self,
        packets: Sender<PacketReceiveEvent<Config>>,
        disconnections: Sender<DisconnectionEvent<Config>>,
        disconnect_sender: Sender<SocketAddr>,
        idle_timeout: tokio::sync::watch::Receiver<std::time::Duration>,
    ) {
        let RawConnection {
            disconnect_task,
            stream,
            serializer,
            packet_length_serializer,
            packets_rx,
            id,
        } = self.connection;
        let (mut read, write) = match stream.into_split().await {
            Ok(split) => split,
            Err(err) => {
                log::error!("({:?}) Couldn't split stream: {}", id, err);
                return;
            }
        };
        read.set_idle_timeout(idle_timeout);
        tokio::spawn(Self::receive_packets(
            read,
            self.ecs_connection,
            Arc::clone(&serializer),
            Arc::clone(&packet_length_serializer),
            packets,
            disconnections,
            disconnect_sender,
        ));
        tokio::spawn(Self::send_packets(
            write,
            packets_rx,
            serializer,
            packet_length_serializer,
            disconnect_task,
            id,
        ));
    }

    async fn receive_packets(
        mut read: impl ReadStream,
        ecs_conn: ServerConnection<Config>,
        serializer: Arc<ServerSerializer<Config>>,
        packet_length_serializer: Arc<Config::LengthSerializer>,
        packets: Sender<PacketReceiveEvent<Config>>,
        disconnections: Sender<DisconnectionEvent<Config>>,
        disconnect_sender: Sender<SocketAddr>,
    ) {
        let disconnect_task = &ecs_conn.disconnect_task;
        let id = ecs_conn.id();
        let _guard = disconnect_task.clone().drop_guard();
        let packets =
            PacketForwarder::new(packets, Config::Protocol::DATAGRAM, disconnect_task.clone());
        let error = loop {
            tokio::select! {
                biased;
                () = disconnect_task.cancelled() => break ReceiveError::IntentionalDisconnection,
                result = read.receive_with_timestamp(Arc::clone(&serializer), &*packet_length_serializer) => {
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
        if let Err(err) = disconnections
            .send(DisconnectionEvent::<Config> {
                error,
                connection: ecs_conn.clone(),
            })
            .await
        {
            log::debug!("({id:?}) Disconnection receiver closed: {err:?}");
        }
        if let Err(err) = disconnect_sender.send(ecs_conn.peer_addr).await {
            log::debug!("({id:?}) Listener closed: {err}");
        }
    }

    async fn send_packets(
        mut write: impl WriteStream,
        mut packets_rx: Receiver<Config::ServerPacket>,
        serializer: Arc<ServerSerializer<Config>>,
        packet_length_serializer: Arc<Config::LengthSerializer>,
        disconnect_task: DisconnectTask,
        id: ConnectionId,
    ) {
        let _guard = disconnect_task.clone().drop_guard();
        let sending = async {
            while let Some(packet) = packets_rx.recv().await {
                if disconnect_task.is_cancelled() {
                    break;
                }
                log::trace!("({id:?}) Sending packet {packet:?}");
                if let Err(err) = write
                    .send(packet, Arc::clone(&serializer), &*packet_length_serializer)
                    .await
                {
                    log::error!("({id:?}) Error sending packet: {err}");
                    break;
                }
            }
        };
        tokio::select! {
            biased;
            () = disconnect_task.cancelled() => {},
            () = sending => {},
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
