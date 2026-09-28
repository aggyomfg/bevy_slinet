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
use tokio_util::sync::CancellationToken;

use crate::connection::transport::{install_idle_timeout, PendingPacket};
use crate::connection::{
    ConnectionId, EcsConnection, MaxPacketSize, NetworkQueueSettings, OutgoingReceiver,
    OutgoingSender, PacketForwarder, RawConnection, ReceiveLimits,
};
use crate::packet_queue::{lossy_channel, LossyReceiver, LossySender};
use crate::protocols::protocol::{
    NetworkStream, PacketReader, PacketWriter, QueueDropReason, ReceiveError,
};
use crate::serializers::serializer::Serializer;
use crate::{ClientConfig, Protocol, SystemSets};

/// Client-side connection to a server.
pub type ClientConnection<Config> = EcsConnection<
    <Config as ClientConfig>::ClientPacket,
    <<Config as ClientConfig>::Protocol as Protocol>::Handle,
>;
type RawClientConnection<Config> = RawConnection<
    <Config as ClientConfig>::ServerPacket,
    <Config as ClientConfig>::ClientPacket,
    <<Config as ClientConfig>::Protocol as Protocol>::ClientStream,
    <Config as ClientConfig>::EncodeError,
    <Config as ClientConfig>::DecodeError,
    <Config as ClientConfig>::LengthSerializer,
>;

type ClientSerializer<Config> = dyn Serializer<
    <Config as ClientConfig>::ServerPacket,
    <Config as ClientConfig>::ClientPacket,
    EncodeError = <Config as ClientConfig>::EncodeError,
    DecodeError = <Config as ClientConfig>::DecodeError,
>;

/// List of client-side connections to a server.
#[derive(Resource)]
pub struct ClientConnections<Config: ClientConfig>(Vec<ClientConnection<Config>>);
impl<Config: ClientConfig> ClientConnections<Config> {
    const fn new() -> Self {
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
        let idle_timeout = install_idle_timeout(app);

        app.init_resource::<ReceiveLimits>()
            .insert_resource(ClientConnections::<Config>::new())
            .add_systems(
                Startup,
                MaxPacketSize::warning_system.in_set(SystemSets::MaxPacketSizeWarning),
            )
            .add_systems(
                Startup,
                MaxPacketSize::set_system.in_set(SystemSets::SetMaxPacketSize),
            )
            .add_systems(
                Update,
                MaxPacketSize::set_system.in_set(SystemSets::SetMaxPacketSize),
            )
            .add_systems(
                PreUpdate,
                lifecycle_system::<Config>
                    .in_set(SystemSets::ClientConnectionEstablish)
                    .in_set(SystemSets::ClientConnectionRemove),
            )
            .add_systems(
                PostUpdate,
                packet_receive_system::<Config>.in_set(SystemSets::ClientPacketReceive),
            )
            .add_systems(
                Startup,
                (
                    Self::setup_system(idle_timeout)
                        .after(SystemSets::SetMaxPacketSize)
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
        Self {
            address: None,
            _marker: PhantomData,
        }
    }
}

impl<Config: ClientConfig> ClientPlugin<Config> {
    /// Installs networking without connecting; trigger [`ConnectionRequestEvent`] to connect later.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Requests a connection during startup.
    ///
    /// # Panics
    /// Panics if the address cannot be resolved or resolves to no endpoints.
    #[expect(
        clippy::expect_used,
        reason = "Preserve the documented panicking constructor API"
    )]
    pub fn connect<A>(addr: A) -> Self
    where
        A: ToSocketAddrs,
    {
        Self {
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
    #[expect(
        clippy::expect_used,
        reason = "Preserve the documented panicking constructor API"
    )]
    pub fn new(address: impl ToSocketAddrs) -> Self {
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

impl<Config: ClientConfig> Clone for ConnectionRequestEvent<Config> {
    fn clone(&self) -> Self {
        Self::new(self.address)
    }
}

#[derive(Resource)]
struct ConnectionRequestSender<Config: ClientConfig>(Sender<SocketAddr>, PhantomData<Config>);

#[derive(Resource)]
struct LifecycleReceiver<Config: ClientConfig>(Receiver<ClientLifecycle<Config>>);

enum ClientLifecycle<Config: ClientConfig> {
    Established(ConnectionEstablishEvent<Config>),
    Closed(ConnectionClosed<Config>),
}

#[derive(Resource)]
struct PacketReceiver<Config: ClientConfig> {
    receiver: LossyReceiver<PacketReceiveEvent<Config>>,
}

struct ConnectionClosed<Config: ClientConfig> {
    event: DisconnectionEvent<Config>,
    id: Option<ConnectionId>,
}

impl<Config: ClientConfig> ConnectionClosed<Config> {
    const fn new(
        error: ReceiveError<Config::DecodeError, Config::LengthSerializer>,
        address: SocketAddr,
        id: Option<ConnectionId>,
    ) -> Self {
        Self {
            event: DisconnectionEvent {
                error,
                address,
                connection_id: id,
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
    packets: OutgoingSender<Config::ClientPacket>,
    result: io::Result<RawClientConnection<Config>>,
}

impl<Config: ClientConfig> ClientPlugin<Config> {
    fn setup_system(
        idle_timeout: tokio::sync::watch::Receiver<std::time::Duration>,
    ) -> impl Fn(Commands, Option<Res<NetworkQueueSettings>>, Res<ReceiveLimits>) {
        move |commands, queues, limits: Res<ReceiveLimits>| {
            Self::setup(
                commands,
                idle_timeout.clone(),
                queues.as_deref().copied().unwrap_or_default(),
                limits.clone(),
            );
        }
    }

    fn setup(
        mut commands: Commands,
        idle_timeout: tokio::sync::watch::Receiver<std::time::Duration>,
        queues: NetworkQueueSettings,
        limits: ReceiveLimits,
    ) {
        let (req_tx, req_rx) = queues.incoming_channel();
        commands.insert_resource(ConnectionRequestSender::<Config>(req_tx, PhantomData));

        let (lifecycle_tx, lifecycle_rx) = queues.incoming_channel();
        let (connection_sender, incoming_connections) = queues.incoming_channel();
        let (pack_tx, pack_rx) = lossy_channel(
            queues.receive_capacity.max(1),
            usize::MAX,
            queues.datagram_receive_overflow,
        );
        commands.insert_resource(LifecycleReceiver::<Config>(lifecycle_rx));
        commands.insert_resource(PacketReceiver::<Config> { receiver: pack_rx });
        commands.add_observer(ConnectionRequestSender::<Config>::observe);

        Self::run_async(Self::process_connection_requests(
            req_rx,
            queues,
            limits,
            lifecycle_tx.clone(),
            connection_sender,
        ));
        Self::run_async(Self::process_connections(
            incoming_connections,
            pack_tx,
            lifecycle_tx,
            idle_timeout,
        ));
    }

    async fn process_connection_requests(
        req_rx: Receiver<SocketAddr>,
        queues: NetworkQueueSettings,
        limits: ReceiveLimits,
        lifecycle: Sender<ClientLifecycle<Config>>,
        connection_sender: Sender<ConnectedTransport<Config>>,
    ) {
        let mut warned = false;
        // Bound in-flight handshakes while allowing other endpoints to connect.
        let requests = futures::stream::unfold(req_rx, |mut requests| async move {
            requests.recv().await.map(|address| (address, requests))
        });
        let connections = requests
            .map(|address| {
                let (tx, rx) = queues.outgoing_channel(Config::Protocol::DATAGRAM);
                let limits = limits.clone();
                let serializer = Config::build_serializer();
                serializer.warn_if_stateful_over_datagrams::<Config::Protocol>(&mut warned);
                async move {
                    let result = Self::create_connection(
                        address,
                        Arc::new(serializer),
                        Config::LengthSerializer::default(),
                        rx,
                        limits,
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
        while let Some(attempt) = connections.next().await {
            if !attempt.publish(&lifecycle, &connection_sender).await {
                return;
            }
        }
    }

    async fn process_connections(
        mut incoming_connections: Receiver<ConnectedTransport<Config>>,
        packets: LossySender<PacketReceiveEvent<Config>>,
        lifecycle: Sender<ClientLifecycle<Config>>,
        idle_timeout: tokio::sync::watch::Receiver<std::time::Duration>,
    ) {
        while let Some(connection) = incoming_connections.recv().await {
            connection
                .run(packets.clone(), lifecycle.clone(), idle_timeout.clone())
                .await;
        }
    }

    async fn create_connection(
        addr: SocketAddr,
        serializer: Arc<ClientSerializer<Config>>,
        packet_length_serializer: Config::LengthSerializer,
        packet_rx: OutgoingReceiver<Config::ClientPacket, <Config::Protocol as Protocol>::Handle>,
        limits: ReceiveLimits,
    ) -> io::Result<RawClientConnection<Config>> {
        Ok(RawConnection::with_limits(
            Config::Protocol::connect_to_server(addr).await?,
            serializer,
            packet_length_serializer,
            packet_rx,
            limits,
        ))
    }
}

impl<Config: ClientConfig> ConnectionAttempt<Config> {
    /// Returns false when the establishment receiver has closed and requests must stop.
    async fn publish(
        self,
        lifecycle: &Sender<ClientLifecycle<Config>>,
        connection_sender: &Sender<ConnectedTransport<Config>>,
    ) -> bool {
        let Self {
            address,
            packets,
            result,
        } = self;
        match result {
            Ok(connection) => {
                let ecs_conn = connection.ecs_connection(packets);
                if let Err(err) = lifecycle
                    .send(ClientLifecycle::Established(ConnectionEstablishEvent::<
                        Config,
                    > {
                        address,
                        connection: ecs_conn.clone(),
                    }))
                    .await
                {
                    log::error!("Failed to send connection establishment: {err:?}");
                    return false;
                }
                if let Err(err) = connection_sender
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
                if let Err(send_err) = lifecycle
                    .send(ClientLifecycle::Closed(ConnectionClosed::<Config>::new(
                        ReceiveError::NoConnection(err),
                        address,
                        None,
                    )))
                    .await
                {
                    log::error!("Failed to send disconnection event: {send_err:?}");
                }
            }
        }
        true
    }
}

impl<Config: ClientConfig> ConnectedTransport<Config> {
    async fn run(
        self,
        packets: LossySender<PacketReceiveEvent<Config>>,
        lifecycle: Sender<ClientLifecycle<Config>>,
        idle_timeout: tokio::sync::watch::Receiver<std::time::Duration>,
    ) {
        let RawConnection {
            disconnect_task,
            stream,
            serializer,
            packet_length_serializer,
            packets_rx,
            receive_limits,
            id,
        } = self.connection;
        let peer_addr = stream.peer_addr();
        let (mut read, write) = match stream.into_split().await {
            Ok(split) => split,
            Err(err) => {
                log::error!("({:?}) Couldn't split stream: {}", id, err);
                self.ecs_connection.disconnect_task.cancel();
                let _ = lifecycle
                    .send(ClientLifecycle::Closed(ConnectionClosed::new(
                        ReceiveError::Io(err),
                        peer_addr,
                        Some(id),
                    )))
                    .await;
                return;
            }
        };
        read.set_idle_timeout(idle_timeout);
        let transport = self.ecs_connection.transport().clone();
        tokio::spawn(Self::receive_packets(
            read,
            self.ecs_connection,
            Arc::clone(&serializer),
            Arc::clone(&packet_length_serializer),
            packets,
            lifecycle,
            peer_addr,
            receive_limits,
        ));
        tokio::spawn(Self::send_packets(
            write,
            packets_rx,
            serializer,
            packet_length_serializer,
            disconnect_task,
            id,
            transport,
        ));
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Receive task needs transport, queue and lifecycle endpoints"
    )]
    async fn receive_packets(
        mut read: impl PacketReader,
        ecs_conn: ClientConnection<Config>,
        serializer: Arc<ClientSerializer<Config>>,
        packet_length_serializer: Arc<Config::LengthSerializer>,
        packets: LossySender<PacketReceiveEvent<Config>>,
        lifecycle: Sender<ClientLifecycle<Config>>,
        peer_addr: SocketAddr,
        receive_limits: ReceiveLimits,
    ) {
        let disconnect_task = &ecs_conn.disconnect_task;
        let id = ecs_conn.id();
        let _guard = disconnect_task.clone().drop_guard();
        let packets = PacketForwarder::new(
            packets,
            Config::Protocol::DATAGRAM,
            disconnect_task.clone(),
            ecs_conn.transport().clone(),
        );
        let error = loop {
            tokio::select! {
                biased;
                () = disconnect_task.cancelled() => break ReceiveError::IntentionalDisconnection,
                result = read.receive_with_timestamp(Arc::clone(&serializer), &*packet_length_serializer, &receive_limits) => {
                    match result {
                        Ok((packet, received_at)) => {
                            log::trace!("({id:?}) Received packet {packet:?}");
                            if !packets.forward(PacketReceiveEvent::<Config> {
                                connection: ecs_conn.clone(),
                                packet,
                                received_at,
                            }, |discarded| {
                                discarded.connection.record_drop(QueueDropReason::ReceiveQueueEvicted);
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
        if let Err(err) = lifecycle
            .send(ClientLifecycle::Closed(ConnectionClosed::<Config>::new(
                error,
                peer_addr,
                Some(id),
            )))
            .await
        {
            log::debug!("({id:?}) Disconnection receiver closed: {err:?}");
        }
    }

    async fn send_packets(
        mut write: impl PacketWriter,
        mut packets_rx: OutgoingReceiver<
            Config::ClientPacket,
            <Config::Protocol as Protocol>::Handle,
        >,
        serializer: Arc<ClientSerializer<Config>>,
        packet_length_serializer: Arc<Config::LengthSerializer>,
        disconnect_task: CancellationToken,
        id: ConnectionId,
        transport: <Config::Protocol as Protocol>::Handle,
    ) {
        let _guard = disconnect_task.clone().drop_guard();
        let sending = async {
            while let Some(packet) = packets_rx.recv().await {
                let mut pending = PendingPacket::new(transport.clone());
                if disconnect_task.is_cancelled() {
                    break;
                }
                log::trace!("({id:?}) Sending packet {packet:?}");
                let result = write
                    .send(packet, Arc::clone(&serializer), &*packet_length_serializer)
                    .await;
                pending.finish(&result);
                if let Err(err) = result {
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

fn packet_receive_system<Config: ClientConfig>(
    mut packets: ResMut<PacketReceiver<Config>>,
    queues: Option<Res<NetworkQueueSettings>>,
    mut commands: Commands,
) {
    let budget = queues
        .as_deref()
        .copied()
        .unwrap_or_default()
        .events_per_frame;
    for _ in 0..budget {
        let next = packets.receiver.try_recv_if(|packet| {
            packet.connection.is_published()
                || (Config::Protocol::DATAGRAM && packet.connection.disconnect_task.is_cancelled())
        });
        let Ok(packet) = next else { break };
        if Config::Protocol::DATAGRAM && packet.connection.disconnect_task.is_cancelled() {
            packet
                .connection
                .record_drop(QueueDropReason::ClosedBeforeDelivery);
        } else if packet.connection.is_published() {
            if Config::Protocol::DATAGRAM {
                commands.queue(move |world: &mut World| {
                    if packet.connection.disconnect_task.is_cancelled() {
                        packet
                            .connection
                            .record_drop(QueueDropReason::ClosedBeforeDelivery);
                    } else {
                        world.trigger(packet);
                    }
                });
            } else {
                commands.trigger(packet);
            }
        }
    }
}

fn lifecycle_system<Config: ClientConfig>(
    mut lifecycle: ResMut<LifecycleReceiver<Config>>,
    mut connections: ResMut<ClientConnections<Config>>,
    queues: Option<Res<NetworkQueueSettings>>,
    mut commands: Commands,
) {
    let budget = queues
        .as_deref()
        .copied()
        .unwrap_or_default()
        .events_per_frame;
    for _ in 0..budget {
        match lifecycle.0.try_recv() {
            Ok(ClientLifecycle::Established(event)) => {
                commands.insert_resource(event.connection.clone());
                event.connection.mark_published();
                connections.register(event.connection.clone());
                commands.trigger(event);
            }
            Ok(ClientLifecycle::Closed(closed)) => {
                if let Some(id) = closed.id {
                    commands.remove_resource::<ClientConnection<Config>>();
                    if let Some(connection) = connections.remove_connection(id) {
                        commands.insert_resource(connection);
                    }
                }
                commands.trigger(closed.event);
            }
            Err(_) => break,
        }
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
    /// Local identity of the closed connection; absent for failed attempts.
    pub connection_id: Option<ConnectionId>,
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
    /// Custom protocols use [`PacketReader::receive_with_timestamp`] semantics.
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

#[cfg(all(test, feature = "protocol_udp", feature = "serializer_bitcode_serde"))]
mod udp_lifecycle_tests {
    use super::*;
    use crate::connection::OverflowPolicy;
    use crate::packet_queue::lossy_channel;
    use crate::protocols::protocol::{FramedWriter, WriteStream};
    use crate::protocols::udp::{UdpConnectionHandle, UdpProtocol};
    use crate::serializers::bitcode_serde::BitcodeSerdeSerializer;
    use crate::serializers::packet_length_serializer::LittleEndian;
    use crate::serializers::serializer::SerializerAdapter;

    struct Config;
    impl ClientConfig for Config {
        type ClientPacket = u8;
        type ServerPacket = u8;
        type Protocol = UdpProtocol;
        type EncodeError = bitcode::Error;
        type DecodeError = bitcode::Error;
        type LengthSerializer = LittleEndian<u32>;
        fn build_serializer() -> SerializerAdapter<u8, u8, bitcode::Error, bitcode::Error> {
            SerializerAdapter::ReadOnly(Arc::new(BitcodeSerdeSerializer))
        }
    }

    #[derive(Default, Resource)]
    struct PacketEvents(usize);

    #[test]
    fn cancelled_udp_packet_is_dropped_while_close_waits_behind_establish() {
        let settings = NetworkQueueSettings {
            events_per_frame: 1,
            ..Default::default()
        };
        let (lifecycle_tx, lifecycle_rx) = settings.incoming_channel();
        let (packet_tx, packet_rx) = lossy_channel(1, usize::MAX, OverflowPolicy::DropNewest);
        let (outgoing, _rx) = settings.outgoing_channel::<_, UdpConnectionHandle>(true);
        let udp = UdpConnectionHandle::new(128, None);
        let address: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let connection = EcsConnection {
            disconnect_task: CancellationToken::new(),
            id: ConnectionId::next(),
            published: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            packet_tx: outgoing,
            transport: udp.clone(),
            local_addr: address,
            peer_addr: address,
        };
        assert!(lifecycle_tx
            .try_send(ClientLifecycle::Established(ConnectionEstablishEvent {
                address,
                connection: connection.clone(),
            }))
            .is_ok());
        assert!(lifecycle_tx
            .try_send(ClientLifecycle::Closed(ConnectionClosed::new(
                ReceiveError::IntentionalDisconnection,
                address,
                Some(connection.id()),
            )))
            .is_ok());
        assert!(packet_tx
            .try_send(
                PacketReceiveEvent {
                    connection: connection.clone(),
                    packet: 7,
                    received_at: Instant::now(),
                },
                1
            )
            .is_ok());
        connection.disconnect();

        let mut app = App::new();
        app.insert_resource(settings);
        app.insert_resource(ClientConnections::<Config>::new());
        app.insert_resource(LifecycleReceiver::<Config>(lifecycle_rx));
        app.insert_resource(PacketReceiver::<Config> {
            receiver: packet_rx,
        });
        app.insert_resource(PacketEvents::default());
        app.add_systems(PreUpdate, lifecycle_system::<Config>);
        app.add_systems(PostUpdate, packet_receive_system::<Config>);
        app.add_observer(
            |_: On<PacketReceiveEvent<Config>>, mut events: ResMut<PacketEvents>| {
                events.0 += 1;
            },
        );

        app.update();
        assert_eq!(app.world().resource::<ClientConnections<Config>>().len(), 1);
        assert_eq!(app.world().resource::<PacketEvents>().0, 0);
        assert_eq!(udp.stats().dropped_closed_before_delivery, 1);
        app.update();
        assert!(app
            .world()
            .resource::<ClientConnections<Config>>()
            .is_empty());
        assert_eq!(app.world().resource::<PacketEvents>().0, 0);
    }

    struct PendingWrite(Arc<std::sync::atomic::AtomicBool>);

    #[async_trait::async_trait]
    impl WriteStream for PendingWrite {
        async fn write_all(&mut self, _buffer: &[u8]) -> io::Result<()> {
            self.0.store(true, std::sync::atomic::Ordering::Release);
            std::future::pending::<io::Result<()>>().await
        }
    }

    #[tokio::test]
    async fn cancelled_paced_write_and_queued_packet_are_both_counted() {
        let settings = NetworkQueueSettings {
            send_capacity: 2,
            ..Default::default()
        };
        let (packet_tx, mut packets_rx) = settings.outgoing_channel::<_, UdpConnectionHandle>(true);
        let udp = UdpConnectionHandle::new(128, None);
        packets_rx.set_transport(udp.clone());
        let address: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let connection = EcsConnection {
            disconnect_task: CancellationToken::new(),
            id: ConnectionId::next(),
            published: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            packet_tx,
            transport: udp.clone(),
            local_addr: address,
            peer_addr: address,
        };
        connection.send(7).unwrap();
        connection.send(8).unwrap();
        let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let task = tokio::spawn(ConnectedTransport::<Config>::send_packets(
            FramedWriter::new(PendingWrite(Arc::clone(&entered))),
            packets_rx,
            Arc::new(Config::build_serializer()),
            Arc::new(LittleEndian::<u32>::default()),
            connection.disconnect_task.clone(),
            connection.id(),
            udp.clone(),
        ));
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !entered.load(std::sync::atomic::Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        connection.disconnect();
        task.await.unwrap();
        assert_eq!(udp.stats().dropped_closed_before_delivery, 2);
    }

    #[test]
    fn udp_observer_disconnect_drops_later_buffered_packet() {
        let settings = NetworkQueueSettings {
            events_per_frame: 2,
            ..Default::default()
        };
        let (lifecycle_tx, lifecycle_rx) = settings.incoming_channel();
        let (packet_tx, packet_rx) = lossy_channel(2, usize::MAX, OverflowPolicy::DropNewest);
        let (outgoing, _rx) = settings.outgoing_channel::<_, UdpConnectionHandle>(true);
        let udp = UdpConnectionHandle::new(128, None);
        let address: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let connection = EcsConnection {
            disconnect_task: CancellationToken::new(),
            id: ConnectionId::next(),
            published: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            packet_tx: outgoing,
            transport: udp.clone(),
            local_addr: address,
            peer_addr: address,
        };
        assert!(lifecycle_tx
            .try_send(ClientLifecycle::Established(ConnectionEstablishEvent {
                address,
                connection: connection.clone(),
            }))
            .is_ok());
        for packet in [1, 2] {
            assert!(packet_tx
                .try_send(
                    PacketReceiveEvent {
                        connection: connection.clone(),
                        packet,
                        received_at: Instant::now(),
                    },
                    1
                )
                .is_ok());
        }

        let mut app = App::new();
        app.insert_resource(settings);
        app.insert_resource(ClientConnections::<Config>::new());
        app.insert_resource(LifecycleReceiver::<Config>(lifecycle_rx));
        app.insert_resource(PacketReceiver::<Config> {
            receiver: packet_rx,
        });
        app.insert_resource(PacketEvents::default());
        app.add_systems(PreUpdate, lifecycle_system::<Config>);
        app.add_systems(PostUpdate, packet_receive_system::<Config>);
        app.add_observer(
            |event: On<PacketReceiveEvent<Config>>, mut events: ResMut<PacketEvents>| {
                events.0 += 1;
                event.event().connection.disconnect();
            },
        );
        app.update();
        assert_eq!(app.world().resource::<PacketEvents>().0, 1);
        assert_eq!(udp.stats().dropped_closed_before_delivery, 1);
    }
}

#[cfg(all(test, feature = "protocol_tcp", feature = "serializer_bitcode_serde"))]
mod tcp_lifecycle_tests {
    use super::*;
    use crate::packet_queue::lossy_channel;
    use crate::protocols::tcp::TcpProtocol;
    use crate::serializers::bitcode_serde::BitcodeSerdeSerializer;
    use crate::serializers::packet_length_serializer::LittleEndian;
    use crate::serializers::serializer::SerializerAdapter;

    struct Config;
    impl ClientConfig for Config {
        type ClientPacket = u8;
        type ServerPacket = u8;
        type Protocol = TcpProtocol;
        type EncodeError = bitcode::Error;
        type DecodeError = bitcode::Error;
        type LengthSerializer = LittleEndian<u32>;
        fn build_serializer() -> SerializerAdapter<u8, u8, bitcode::Error, bitcode::Error> {
            SerializerAdapter::ReadOnly(Arc::new(BitcodeSerdeSerializer))
        }
    }

    #[derive(Default, Resource)]
    struct Packets(Vec<u8>);

    #[test]
    fn final_tcp_packet_survives_registry_removal_in_same_frame() {
        let settings = NetworkQueueSettings {
            events_per_frame: 2,
            ..Default::default()
        };
        let (lifecycle_tx, lifecycle_rx) = settings.incoming_channel();
        let (packet_tx, packet_rx) =
            lossy_channel(2, usize::MAX, settings.datagram_receive_overflow);
        let (outgoing, _rx) = settings.outgoing_channel::<_, ()>(false);
        let address: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let connection = EcsConnection {
            disconnect_task: CancellationToken::new(),
            id: ConnectionId::next(),
            published: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            packet_tx: outgoing,
            transport: (),
            local_addr: address,
            peer_addr: address,
        };
        assert!(lifecycle_tx
            .try_send(ClientLifecycle::Established(ConnectionEstablishEvent {
                address,
                connection: connection.clone(),
            }))
            .is_ok());
        assert!(lifecycle_tx
            .try_send(ClientLifecycle::Closed(ConnectionClosed::new(
                ReceiveError::IntentionalDisconnection,
                address,
                Some(connection.id()),
            )))
            .is_ok());
        assert!(packet_tx
            .try_send(
                PacketReceiveEvent {
                    connection: connection.clone(),
                    packet: 9,
                    received_at: Instant::now(),
                },
                1
            )
            .is_ok());
        connection.disconnect();

        let mut app = App::new();
        app.insert_resource(settings);
        app.insert_resource(ClientConnections::<Config>::new());
        app.insert_resource(LifecycleReceiver::<Config>(lifecycle_rx));
        app.insert_resource(PacketReceiver::<Config> {
            receiver: packet_rx,
        });
        app.insert_resource(Packets::default());
        app.add_systems(PreUpdate, lifecycle_system::<Config>);
        app.add_systems(PostUpdate, packet_receive_system::<Config>);
        app.add_observer(
            |event: On<PacketReceiveEvent<Config>>, mut packets: ResMut<Packets>| {
                packets.0.push(event.event().packet);
            },
        );
        app.update();
        assert!(app
            .world()
            .resource::<ClientConnections<Config>>()
            .is_empty());
        assert_eq!(app.world().resource::<Packets>().0, [9]);
    }
}
