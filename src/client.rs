//! Client part of the plugin. You can enable it by adding `client` feature.

use std::future::Future;
use std::io;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::sync::Arc;

use bevy::ecs::system::SystemParam;
use bevy::log;
use bevy::platform::time::Instant;
use bevy::prelude::*;
use futures::StreamExt;
use tokio::sync::mpsc::{Receiver, Sender};

use crate::connection::{
    max_packet_size_warning_system, set_max_packet_size_system, ConnectionId, EcsConnection,
    NetworkQueueSettings, PacketForwarder, RawConnection,
};
use crate::protocol::ReadStream;
use crate::protocol::WriteStream;
use crate::protocol::{NetworkStream, ReceiveError};
use crate::serializer::Serializer;
use crate::{ClientConfig, Protocol, SystemSets};

/// Client-side connection to a server.
pub type ClientConnection<Config> = EcsConnection<<Config as ClientConfig>::ClientPacket>;
type RawClientConnection<Config> = RawConnection<
    <Config as ClientConfig>::ServerPacket,
    <Config as ClientConfig>::ClientPacket,
    <<Config as ClientConfig>::Protocol as Protocol>::ClientStream,
    <Config as ClientConfig>::EncodeError,
    <Config as ClientConfig>::DecodeError,
    <Config as ClientConfig>::LengthSerializer,
>;

/// List of client-side connections to a server.
#[derive(Resource)]
pub struct ClientConnections<Config: ClientConfig>(Vec<ClientConnection<Config>>);
impl<Config: ClientConfig> ClientConnections<Config> {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn register(&mut self, connection: ClientConnection<Config>) {
        self.0.push(connection);
    }

    fn remove_connection(&mut self, id: ConnectionId) -> Option<ClientConnection<Config>> {
        self.0.retain(|connection| connection.id() != id);
        self.0.last().cloned()
    }
}

impl<Config: ClientConfig> std::ops::Deref for ClientConnections<Config> {
    type Target = Vec<ClientConnection<Config>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<Config: ClientConfig> std::ops::DerefMut for ClientConnections<Config> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Client-side plugin. Use [`ClientPlugin::connect`] to connect immediately or
/// [`ClientPlugin::new`] to add the required systems and send [`ConnectionRequestEvent`] later.
pub struct ClientPlugin<Config: ClientConfig> {
    address: Option<SocketAddr>,
    _marker: PhantomData<Config>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, SystemSet)]
struct AddInitialConnectionRequestEventLabel;

impl<Config: ClientConfig> Plugin for ClientPlugin<Config> {
    fn build(&self, app: &mut App) {
        let address = self.address;
        #[cfg(feature = "protocol_udp")]
        let idle_timeout = crate::protocols::udp::IdleTimeoutSettings::install(app);
        #[cfg(not(feature = "protocol_udp"))]
        let idle_timeout = tokio::sync::watch::channel(std::time::Duration::MAX).1;

        app.insert_resource(ClientConnections::<Config>::new())
            .add_systems(
                Startup,
                max_packet_size_warning_system.in_set(SystemSets::MaxPacketSizeWarning),
            )
            .add_systems(
                Update,
                set_max_packet_size_system.in_set(SystemSets::SetMaxPacketSize),
            )
            .add_systems(
                PreUpdate,
                (connection_establish_system::<Config>
                    .in_set(SystemSets::ClientConnectionEstablish),),
            )
            .add_systems(
                PostUpdate,
                (
                    connection_remove_system::<Config>.in_set(SystemSets::ClientConnectionRemove),
                    packet_receive_system::<Config>.in_set(SystemSets::ClientPacketReceive),
                ),
            )
            .add_systems(
                Startup,
                (
                    Self::setup_system(idle_timeout).before(AddInitialConnectionRequestEventLabel),
                    (move |mut commands: Commands| {
                        if let Some(address) = address {
                            commands.trigger(ConnectionRequestEvent::<Config>::new(address));
                        }
                    })
                    .in_set(AddInitialConnectionRequestEventLabel),
                ),
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
    /// Installs networking without connecting; trigger [`ConnectionRequestEvent`] to connect later.
    pub fn new() -> ClientPlugin<Config> {
        ClientPlugin::default()
    }

    /// Requests a connection during startup.
    ///
    /// # Panics
    /// Panics if the address cannot be resolved or resolves to no endpoints.
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
#[derive(Event)]
pub struct ConnectionRequestEvent<Config: ClientConfig> {
    address: SocketAddr,
    _marker: PhantomData<Config>,
}

impl<Config: ClientConfig> ConnectionRequestEvent<Config> {
    /// Resolves the first endpoint for a connection request.
    ///
    /// # Panics
    /// Panics if the address cannot be resolved or resolves to no endpoints.
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
        ConnectionRequestEvent::new(self.address)
    }
}

#[derive(Resource)]
struct ConnectionRequestSender<Config: ClientConfig>(Sender<SocketAddr>, PhantomData<Config>);

#[derive(Resource)]
struct ConnectionReceiver<Config: ClientConfig>(Receiver<ConnectionEstablishEvent<Config>>);

#[derive(Resource)]
struct DisconnectionReceiver<Config: ClientConfig>(Receiver<ConnectionClosed<Config>>);

#[derive(Resource)]
struct PacketReceiver<Config: ClientConfig>(Receiver<PacketReceiveEvent<Config>>);

struct ConnectionClosed<Config: ClientConfig> {
    event: DisconnectionEvent<Config>,
    id: Option<ConnectionId>,
}

impl<Config: ClientConfig> ConnectionClosed<Config> {
    fn new(
        error: ReceiveError<Config::DecodeError, Config::LengthSerializer>,
        address: SocketAddr,
        id: Option<ConnectionId>,
    ) -> Self {
        Self {
            event: DisconnectionEvent {
                error,
                address,
                _marker: PhantomData,
            },
            id,
        }
    }
}

struct ConnectedTransport<Config: ClientConfig> {
    connection: RawClientConnection<Config>,
    ecs_connection: ClientConnection<Config>,
}

struct ConnectionAttempt<Config: ClientConfig> {
    address: SocketAddr,
    packets: Sender<Config::ClientPacket>,
    result: io::Result<RawClientConnection<Config>>,
}

impl<Config: ClientConfig> ClientPlugin<Config> {
    fn setup_system(
        idle_timeout: tokio::sync::watch::Receiver<std::time::Duration>,
    ) -> impl Fn(Commands, Option<Res<NetworkQueueSettings>>) {
        move |commands, queues| {
            Self::setup(
                commands,
                idle_timeout.clone(),
                queues.as_deref().copied().unwrap_or_default(),
            )
        }
    }

    fn setup(
        mut commands: Commands,
        idle_timeout: tokio::sync::watch::Receiver<std::time::Duration>,
        queues: NetworkQueueSettings,
    ) {
        let (req_tx, req_rx) = queues.incoming_channel();
        commands.insert_resource(ConnectionRequestSender::<Config>(req_tx, PhantomData));

        let (conn_tx, conn_rx) = queues.incoming_channel();
        let (conn_tx2, mut conn_rx2) = queues.incoming_channel();
        let (disc_tx, disc_rx) = queues.incoming_channel();
        let (pack_tx, pack_rx) = queues.incoming_channel();
        commands.insert_resource(ConnectionReceiver::<Config>(conn_rx));
        commands.insert_resource(DisconnectionReceiver::<Config>(disc_rx));
        commands.insert_resource(PacketReceiver::<Config>(pack_rx));
        commands.add_observer(ConnectionRequestSender::<Config>::observe);

        let disc_tx2 = disc_tx.clone();
        Self::run_async(async move {
            let mut warned = false;
            // Bound in-flight handshakes while allowing other endpoints to connect.
            let requests = futures::stream::unfold(req_rx, |mut requests| async move {
                requests.recv().await.map(|address| (address, requests))
            });
            let connections = requests
                .map(|address| {
                    let (tx, rx) = queues.outgoing_channel();
                    let serializer = Config::build_serializer();
                    serializer.warn_if_stateful_over_datagrams::<Config::Protocol>(&mut warned);
                    async move {
                        let result = Self::create_connection(
                            address,
                            Arc::new(serializer),
                            Config::LengthSerializer::default(),
                            rx,
                        )
                        .await;
                        ConnectionAttempt::<Config> {
                            address,
                            packets: tx,
                            result,
                        }
                    }
                })
                .buffer_unordered(8);
            futures::pin_mut!(connections);
            while let Some(ConnectionAttempt {
                address,
                packets,
                result,
            }) = connections.next().await
            {
                match result {
                    Ok(connection) => {
                        let ecs_conn = EcsConnection {
                            disconnect_task: connection.disconnect_task.clone(),
                            id: connection.id(),
                            packet_tx: packets,
                            local_addr: connection.local_addr(),
                            peer_addr: connection.peer_addr(),
                        };
                        if let Err(err) = conn_tx
                            .send(ConnectionEstablishEvent::<Config> {
                                address,
                                connection: ecs_conn.clone(),
                            })
                            .await
                        {
                            log::error!("Failed to send connection establishment: {err:?}");
                            return;
                        }
                        if let Err(err) = conn_tx2
                            .send(ConnectedTransport::<Config> {
                                connection,
                                ecs_connection: ecs_conn,
                            })
                            .await
                        {
                            log::error!("Failed to send raw connection: {err:?}");
                        }
                    }
                    Err(err) => {
                        log::warn!("Couldn't connect to server: {err:?}");
                        if let Err(send_err) = disc_tx2
                            .send(ConnectionClosed::<Config>::new(
                                ReceiveError::NoConnection(err),
                                address,
                                None,
                            ))
                            .await
                        {
                            log::error!("Failed to send disconnection event: {send_err:?}");
                        }
                    }
                }
            }
        });

        Self::run_async(async move {
            while let Some(ConnectedTransport {
                connection,
                ecs_connection: ecs_conn,
            }) = conn_rx2.recv().await
            {
                let RawConnection {
                    disconnect_task,
                    stream,
                    serializer,
                    packet_length_serializer,
                    mut packets_rx,
                    id,
                } = connection;
                let pack_tx2 = pack_tx.clone();
                let disc_tx2 = disc_tx.clone();
                let serializer2 = Arc::clone(&serializer);
                let packet_length_serializer2 = Arc::clone(&packet_length_serializer);
                let peer_addr = stream.peer_addr();

                let (mut read, mut write) = match stream.into_split().await {
                    Ok(split) => split,
                    Err(err) => {
                        log::error!("({:?}) Couldn't split stream: {}", id, err);
                        continue;
                    }
                };

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
                            _ = disconnect_task.cancelled() => break ReceiveError::IntentionalDisconnection,
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
                    if let Err(err) = disc_tx2
                        .send(ConnectionClosed::<Config>::new(error, peer_addr, Some(id)))
                        .await
                    {
                        log::debug!("({id:?}) Disconnection receiver closed: {err:?}");
                    }
                });
                tokio::spawn(async move {
                    let _guard = write_cancel.clone().drop_guard();
                    let sending = async {
                        while let Some(packet) = packets_rx.recv().await {
                            if write_cancel.is_cancelled() {
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
                        _ = write_cancel.cancelled() => {},
                        _ = sending => {},
                    }
                });
            }
        });
    }

    async fn create_connection(
        addr: SocketAddr,
        serializer: Arc<
            dyn Serializer<
                Config::ServerPacket,
                Config::ClientPacket,
                EncodeError = Config::EncodeError,
                DecodeError = Config::DecodeError,
            >,
        >,
        packet_length_serializer: Config::LengthSerializer,
        packet_rx: Receiver<Config::ClientPacket>,
    ) -> io::Result<RawClientConnection<Config>> {
        Ok(RawConnection::new(
            Config::Protocol::connect_to_server(addr).await?,
            serializer,
            packet_length_serializer,
            packet_rx,
        ))
    }
}

impl<Config: ClientConfig> ConnectionRequestSender<Config> {
    fn request(&self, address: SocketAddr) {
        if let Err(err) = self.0.try_send(address) {
            log::error!("Failed to send connection request: {err:?}");
        }
    }

    fn observe(connection_request: On<ConnectionRequestEvent<Config>>, requests: Res<Self>) {
        requests.request(connection_request.event().address);
    }
}

/// Received packets and the frame budget governing their delivery to observers.
#[derive(SystemParam)]
struct IncomingPackets<'w, Config: ClientConfig> {
    packets: ResMut<'w, PacketReceiver<Config>>,
    queues: Option<Res<'w, NetworkQueueSettings>>,
}

impl<Config: ClientConfig> IncomingPackets<'_, Config> {
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

/// Completed connection attempts and the frame budget for registering them.
#[derive(SystemParam)]
struct IncomingConnections<'w, Config: ClientConfig> {
    connections: ResMut<'w, ConnectionReceiver<Config>>,
    queues: Option<Res<'w, NetworkQueueSettings>>,
}

impl<Config: ClientConfig> IncomingConnections<'_, Config> {
    fn drain(&mut self) -> impl Iterator<Item = ConnectionEstablishEvent<Config>> + '_ {
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

/// Closed connections and the frame budget for notifying observers.
#[derive(SystemParam)]
struct IncomingDisconnections<'w, Config: ClientConfig> {
    disconnections: ResMut<'w, DisconnectionReceiver<Config>>,
    queues: Option<Res<'w, NetworkQueueSettings>>,
}

impl<Config: ClientConfig> IncomingDisconnections<'_, Config> {
    fn drain(&mut self) -> impl Iterator<Item = ConnectionClosed<Config>> + '_ {
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

fn packet_receive_system<Config: ClientConfig>(
    mut packets: IncomingPackets<Config>,
    mut commands: Commands,
) {
    for packet in packets.drain() {
        commands.trigger(packet);
    }
}

fn connection_establish_system<Config: ClientConfig>(
    mut commands: Commands,
    mut incoming: IncomingConnections<Config>,
    mut connections: ResMut<ClientConnections<Config>>,
) {
    for event in incoming.drain() {
        commands.insert_resource(event.connection.clone());
        connections.register(event.connection.clone());
        commands.trigger(event);
    }
}

fn connection_remove_system<Config: ClientConfig>(
    mut commands: Commands,
    mut incoming: IncomingDisconnections<Config>,
    mut connections: ResMut<ClientConnections<Config>>,
) {
    for closed in incoming.drain() {
        if let Some(id) = closed.id {
            commands.remove_resource::<ClientConnection<Config>>();
            if let Some(connection) = connections.remove_connection(id) {
                commands.insert_resource(connection);
            }
        }
        commands.trigger(closed.event);
    }
}

/// Indicates that a connection was successfully established.
#[derive(Event)]
#[non_exhaustive]
pub struct ConnectionEstablishEvent<Config: ClientConfig> {
    /// A server address.
    pub address: SocketAddr,
    /// The connection.
    pub connection: ClientConnection<Config>,
}

/// Reports a failed connection attempt or the closure of an established connection.
#[derive(Event)]
pub struct DisconnectionEvent<Config: ClientConfig> {
    /// The error.
    pub error: ReceiveError<Config::DecodeError, Config::LengthSerializer>,
    /// A server's IP address.
    pub address: SocketAddr,
    _marker: PhantomData<Config>,
}

/// Sent for every packet received.
#[derive(Event)]
#[non_exhaustive]
pub struct PacketReceiveEvent<Config: ClientConfig> {
    /// The connection.
    pub connection: ClientConnection<Config>,
    /// The packet.
    pub packet: Config::ServerPacket,
    /// When the built-in transport finished reading the packet, before decoding or queueing.
    /// Custom protocols use [`ReadStream::receive_with_timestamp`] semantics.
    pub received_at: Instant,
}

impl<Config: ClientConfig> ClientPlugin<Config> {
    #[cfg(not(target_family = "wasm"))]
    fn run_async<F>(future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        std::thread::spawn(move || {
            let runtime_result = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build();

            let runtime = match runtime_result {
                Ok(rt) => rt,
                Err(err) => {
                    log::error!("Failed to create tokio runtime: {:?}", err);
                    return;
                }
            };

            runtime.block_on(async move {
                let local = tokio::task::LocalSet::new();
                local
                    .run_until(async move {
                        if let Err(err) = tokio::task::spawn_local(future).await {
                            log::error!("Failed to run async task: {}", err);
                        }
                    })
                    .await;
            });
        });
    }

    #[cfg(target_family = "wasm")]
    fn run_async<F>(future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        wasm_bindgen_futures::spawn_local(async move {
            let local = tokio::task::LocalSet::new();
            local
                .run_until(async move {
                    if let Err(err) = tokio::task::spawn_local(future).await {
                        log::error!("Failed to run async task: {:?}", err);
                    }
                })
                .await;
        });
    }
}
