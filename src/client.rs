//! Client part of the plugin. You can enable it by adding `client` feature.

use std::future::Future;
use std::io;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::net::ToSocketAddrs;
use std::sync::Arc;

use bevy::log;
use bevy::platform::time::Instant;
use bevy::prelude::*;
use futures::StreamExt;
use tokio::sync::mpsc::{Receiver, Sender};

use crate::connection::{
    forward_packet, max_packet_size_warning_system, set_max_packet_size_system,
    warn_if_stateful_over_datagrams, EcsConnection, NetworkQueueSettings, RawConnection,
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
        let idle_timeout = crate::protocols::udp::idle_timeout_receiver(app);
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
                    create_setup_system::<Config>(idle_timeout)
                        .before(AddInitialConnectionRequestEventLabel),
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
    /// Adds the required systems, but doesn't connect immediately.
    /// Create a [ClientConnection] using [`Protocol::connect_to_server`]
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
#[derive(Event)]
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
        ConnectionRequestEvent::new(self.address)
    }
}

#[derive(Resource)]
struct ConnectionRequestSender<Config: ClientConfig>(Sender<SocketAddr>, PhantomData<Config>);

#[derive(Resource)]
struct ConnectionReceiver<Config: ClientConfig>(Receiver<(SocketAddr, ClientConnection<Config>)>);

#[allow(clippy::type_complexity)]
#[derive(Resource)]

struct DisconnectionReceiver<Config: ClientConfig>(
    Receiver<(
        ReceiveError<Config::DecodeError, Config::LengthSerializer>,
        SocketAddr,
        Option<crate::connection::ConnectionId>,
    )>,
    PhantomData<Config>,
);

#[derive(Resource)]
struct PacketReceiver<Config: ClientConfig>(
    Receiver<(ClientConnection<Config>, Config::ServerPacket, Instant)>,
);

fn create_setup_system<Config: ClientConfig>(
    idle_timeout: tokio::sync::watch::Receiver<std::time::Duration>,
) -> impl Fn(Commands, Option<Res<NetworkQueueSettings>>) {
    move |commands, queues| {
        setup_system::<Config>(
            commands,
            idle_timeout.clone(),
            queues.as_deref().copied().unwrap_or_default(),
        )
    }
}

fn setup_system<Config: ClientConfig>(
    mut commands: Commands,
    idle_timeout: tokio::sync::watch::Receiver<std::time::Duration>,
    queues: NetworkQueueSettings,
) {
    let (req_tx, req_rx) = tokio::sync::mpsc::channel(queues.receive_capacity.max(1));
    commands.insert_resource(ConnectionRequestSender::<Config>(req_tx, PhantomData));

    let (conn_tx, conn_rx) = tokio::sync::mpsc::channel(queues.receive_capacity.max(1));
    let (conn_tx2, mut conn_rx2) = tokio::sync::mpsc::channel(queues.receive_capacity.max(1));
    let (disc_tx, disc_rx) = tokio::sync::mpsc::channel(queues.receive_capacity.max(1));
    let (pack_tx, pack_rx) = tokio::sync::mpsc::channel(queues.receive_capacity.max(1));
    commands.insert_resource(ConnectionReceiver::<Config>(conn_rx));
    commands.insert_resource(DisconnectionReceiver::<Config>(disc_rx, PhantomData));
    commands.insert_resource(PacketReceiver::<Config>(pack_rx));
    commands.add_observer(connection_request_system::<Config>);

    // Connection
    let disc_tx2 = disc_tx.clone();
    run_async(async move {
        let mut warned = false;
        // Bound in-flight handshakes while allowing other endpoints to connect.
        let requests = futures::stream::unfold(req_rx, |mut requests| async move {
            requests.recv().await.map(|address| (address, requests))
        });
        let connections = requests
            .map(|address| {
                let (tx, rx) = tokio::sync::mpsc::channel(queues.send_capacity.max(1));
                let serializer = Config::build_serializer();
                warn_if_stateful_over_datagrams::<Config::Protocol, _, _, _, _>(
                    &serializer,
                    &mut warned,
                );
                async move {
                    let result = create_connection::<Config>(
                        address,
                        Arc::new(serializer),
                        Config::LengthSerializer::default(),
                        rx,
                    )
                    .await;
                    (address, tx, result)
                }
            })
            .buffer_unordered(8);
        futures::pin_mut!(connections);
        while let Some((address, tx, result)) = connections.next().await {
            match result {
                Ok(connection) => {
                    let ecs_conn = EcsConnection {
                        disconnect_task: connection.disconnect_task.clone(),
                        id: connection.id(),
                        packet_tx: tx,
                        local_addr: connection.local_addr(),
                        peer_addr: connection.peer_addr(),
                    };
                    if let Err(err) = conn_tx.send((address, ecs_conn.clone())).await {
                        log::error!("Failed to send connection establishment: {err:?}");
                        return;
                    }
                    if let Err(err) = conn_tx2.send((connection, ecs_conn)).await {
                        log::error!("Failed to send raw connection: {err:?}");
                    }
                }
                Err(err) => {
                    log::warn!("Couldn't connect to server: {err:?}");
                    if let Err(send_err) = disc_tx2
                        .send((ReceiveError::NoConnection(err), address, None))
                        .await
                    {
                        log::error!("Failed to send disconnection event: {send_err:?}");
                    }
                }
            }
        }
    });

    run_async(async move {
        while let Some((connection, ecs_conn)) = conn_rx2.recv().await {
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
                loop {
                    tokio::select! {
                        result = read.receive_with_timestamp(Arc::clone(&serializer2), &*packet_length_serializer2) => {
                            match result {
                                Ok((packet, received_at)) => {
                                    log::trace!("({id:?}) Received packet {packet:?}");
                                    if !forward_packet(&pack_tx2, (ecs_conn.clone(), packet, received_at), Config::Protocol::DATAGRAM, &disconnect_task).await {
                                        break
                                    }
                                }
                                Err(err) => {
                                    disconnect_task.cancel();
                                    log::debug!("({id:?}) Error receiving next packet: {err:?}");
                                    if disc_tx2.send((err, peer_addr, Some(id))).await.is_err() {
                                        break
                                    }
                                    break;
                                }
                            }
                        }
                        _ = disconnect_task.cancelled() => {
                            log::debug!("({id:?}) Client disconnected intentionally");
                            if let Err(err) = disc_tx2.send((ReceiveError::IntentionalDisconnection, peer_addr, Some(id))).await {
                                log::error!("({id:?}) Failed to send disconnection event: {err:?}");
                            }
                            break
                        }
                    }
                }
            });
            tokio::spawn(async move {
                let _guard = write_cancel.clone().drop_guard();
                let sending = async {
                    while let Some(packet) = packets_rx.recv().await {
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

pub(crate) async fn create_connection<Config: ClientConfig>(
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

fn connection_request_system<Config: ClientConfig>(
    connection_request: On<ConnectionRequestEvent<Config>>,
    requests: Res<ConnectionRequestSender<Config>>,
) {
    if let Err(err) = requests.0.try_send(connection_request.event().address) {
        log::error!("Failed to send connection request: {err:?}");
    }
}

fn packet_receive_system<Config: ClientConfig>(
    mut packets: ResMut<PacketReceiver<Config>>,
    mut commands: Commands,
    queues: Option<Res<NetworkQueueSettings>>,
) {
    for (connection, packet, received_at) in std::iter::from_fn(|| packets.0.try_recv().ok()).take(
        queues
            .as_deref()
            .copied()
            .unwrap_or_default()
            .events_per_frame,
    ) {
        commands.trigger(PacketReceiveEvent::<Config> {
            connection,
            packet,
            received_at,
        });
    }
}

fn connection_establish_system<Config: ClientConfig>(
    mut commands: Commands,
    mut new_connections: ResMut<ConnectionReceiver<Config>>,
    mut connections: ResMut<ClientConnections<Config>>,
    queues: Option<Res<NetworkQueueSettings>>,
) {
    for (address, connection) in std::iter::from_fn(|| new_connections.0.try_recv().ok()).take(
        queues
            .as_deref()
            .copied()
            .unwrap_or_default()
            .events_per_frame,
    ) {
        commands.insert_resource(connection.clone());
        connections.push(connection.clone());
        commands.trigger(ConnectionEstablishEvent::<Config> {
            address,
            connection,
        });
    }
}

fn connection_remove_system<Config: ClientConfig>(
    mut commands: Commands,
    mut old_connections: ResMut<DisconnectionReceiver<Config>>,
    mut connections: ResMut<ClientConnections<Config>>,
    queues: Option<Res<NetworkQueueSettings>>,
) {
    for (error, address, id) in std::iter::from_fn(|| old_connections.0.try_recv().ok()).take(
        queues
            .as_deref()
            .copied()
            .unwrap_or_default()
            .events_per_frame,
    ) {
        if let Some(id) = id {
            commands.remove_resource::<ClientConnection<Config>>();
            connections.retain(|conn| conn.id() != id);
            if let Some(connection) = connections.last() {
                commands.insert_resource(connection.clone());
            }
        }
        commands.trigger(DisconnectionEvent::<Config> {
            error,
            address,
            _marker: PhantomData,
        });
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

/// Indicates that something went wrong during a connection attempt. See [`DisconnectionEvent::error`] for details
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
    /// Custom protocols use `ReadStream::receive_with_timestamp` semantics.
    pub received_at: Instant,
}

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
