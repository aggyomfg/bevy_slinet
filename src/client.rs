//! Client part of the plugin. You can enable it by adding `client` feature.

use crate::connection::settings::EndpointSetup;
use crate::connection::NetworkSettings;

/// Optional settings for this client config, overriding app-wide defaults.
pub type ClientSettings<Config> = NetworkSettings<ClientPlugin<Config>>;

/// Scheduling phases for one client config; see [`crate::NetworkSystems`].
///
/// ```
/// use bevy::prelude::*;
/// use bevy_slinet::{ClientConfig, client::ClientSystems};
///
/// fn configure<C: ClientConfig>(app: &mut App) {
///     app.add_systems(PreUpdate,
///         consume_network_state.after(ClientSystems::<C>::RECEIVE));
/// }
///
/// fn consume_network_state() { /* Read state populated by packet observers. */ }
/// ```
pub type ClientSystems<Config> = crate::NetworkSystems<ClientPlugin<Config>>;

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

use crate::connection::tasks::{
    run_connections, send_packets, PacketCodecs, ReceiveTaskState, SendTaskState,
};
use crate::connection::{lossy_channel, LossyReceiver, LossySender};
use crate::connection::{
    ConnectionId, EcsConnection, NetworkQueueSettings, OutgoingReceiver, OutgoingSender,
    PacketForwarder, RawConnection, ReceiveLimits,
};
use crate::protocols::protocol::{NetworkStream, PacketReader, QueueDropReason, ReceiveError};
use crate::serializers::serializer::Serializer;
use crate::{ClientConfig, PacketLengthSerializer, Protocol, SystemSets};

/// Client-side connection to a server.
pub type ClientConnection<Config> = EcsConnection<
    <Config as ClientConfig>::ClientPacket,
    <<Config as ClientConfig>::Protocol as Protocol>::Handle,
    ClientPlugin<Config>,
>;
const MAX_CONNECTION_ATTEMPTS: usize = 8;
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

impl<Config: ClientConfig> Plugin for ClientPlugin<Config> {
    fn build(&self, app: &mut App) {
        ClientSystems::<Config>::configure_settings(app);
        app.insert_resource(ClientConnections::<Config>::new())
            .add_systems(
                Startup,
                Self::setup_system.in_set(ClientSystems::<Config>::SETUP),
            );
        if let Some(address) = self.address {
            app.add_systems(
                Startup,
                (move |mut commands: Commands| {
                    commands.trigger(ConnectionRequestEvent::<Config>::new(address));
                })
                .after(ClientSystems::<Config>::SETUP),
            );
        }
        configure_receive_systems::<Config>(app);
    }
}

fn configure_receive_systems<Config: ClientConfig>(app: &mut App) {
    ClientSystems::<Config>::configure_receive(
        app,
        Config::Protocol::DATAGRAM,
        crate::scheduling::ReceiveSets {
            receive: SystemSets::ClientReceive,
            lifecycle: [
                SystemSets::ClientConnectionEstablish,
                SystemSets::ClientConnectionRemove,
            ],
            packets: SystemSets::ClientPacketReceive,
        },
        incoming_system::<Config>,
        receive_datagram_packets::<Config>,
    );
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
struct IncomingReceiver<Config: ClientConfig>(Receiver<IncomingMessage<Config>>);

enum IncomingMessage<Config: ClientConfig> {
    StreamPacket(PacketReceiveEvent<Config>),
    Established(ConnectionEstablishEvent<Config>),
    Closed(ConnectionClosed<Config>),
}

#[derive(Resource)]
struct DatagramPacketReceiver<Config: ClientConfig> {
    receiver: LossyReceiver<PacketReceiveEvent<Config>>,
}

struct ConnectionClosed<Config: ClientConfig> {
    event: DisconnectionEvent<Config>,
    id: Option<ConnectionId>,
}

impl<Config: ClientConfig> ConnectionClosed<Config> {
    const fn new(
        error: ReceiveError<
            Config::DecodeError,
            <Config::LengthSerializer as PacketLengthSerializer>::Error,
        >,
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
        commands: Commands,
        endpoint: EndpointSetup<Self>,
        runtime: Res<crate::runtime::NetworkRuntime>,
    ) {
        Self::setup(
            commands,
            endpoint.queues(),
            endpoint.receive_limits(),
            &runtime,
        );
    }

    fn setup(
        mut commands: Commands,
        queues: NetworkQueueSettings,
        limits: ReceiveLimits,
        runtime: &crate::runtime::NetworkRuntime,
    ) {
        let (connection_request_tx, connection_request_rx) = queues.incoming_channel();
        commands.insert_resource(ConnectionRequestSender::<Config>(
            connection_request_tx,
            PhantomData,
        ));

        let (incoming_tx, incoming_rx) = queues.incoming_channel();
        let (connection_sender, incoming_connections) = queues.incoming_channel();
        let (datagram_packet_tx, datagram_packet_rx) = lossy_channel(
            queues.receive_capacity.max(1),
            usize::MAX,
            queues.datagram_receive_overflow,
        );
        commands.insert_resource(IncomingReceiver::<Config>(incoming_rx));
        if Config::Protocol::DATAGRAM {
            commands.insert_resource(DatagramPacketReceiver::<Config> {
                receiver: datagram_packet_rx,
            });
        }
        commands.add_observer(ConnectionRequestSender::<Config>::observe);

        runtime.spawn(Self::process_connection_requests(
            connection_request_rx,
            queues,
            limits,
            incoming_tx.clone(),
            connection_sender,
        ));
        runtime.spawn(Self::process_connections(
            incoming_connections,
            datagram_packet_tx,
            incoming_tx,
        ));
    }

    async fn process_connection_requests(
        connection_request_rx: Receiver<SocketAddr>,
        queues: NetworkQueueSettings,
        limits: ReceiveLimits,
        incoming: Sender<IncomingMessage<Config>>,
        connection_sender: Sender<ConnectedTransport<Config>>,
    ) {
        let mut warned = false;
        // Bound in-flight connection attempts while allowing other endpoints to connect.
        let requests = futures::stream::unfold(connection_request_rx, |mut requests| async move {
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
            .buffer_unordered(MAX_CONNECTION_ATTEMPTS);
        futures::pin_mut!(connections);
        while let Some(attempt) = connections.next().await {
            if !attempt.publish(&incoming, &connection_sender).await {
                return;
            }
        }
    }

    async fn process_connections(
        incoming_connections: Receiver<ConnectedTransport<Config>>,
        datagram_packets: LossySender<PacketReceiveEvent<Config>>,
        incoming: Sender<IncomingMessage<Config>>,
    ) {
        run_connections(
            incoming_connections,
            MAX_CONNECTION_ATTEMPTS,
            |connection| connection.run(datagram_packets.clone(), incoming.clone()),
        )
        .await;
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
        incoming: &Sender<IncomingMessage<Config>>,
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
                if let Err(err) = incoming
                    .send(IncomingMessage::Established(ConnectionEstablishEvent::<
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
                if let Err(send_err) = incoming
                    .send(IncomingMessage::Closed(ConnectionClosed::<Config>::new(
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
        datagram_packets: LossySender<PacketReceiveEvent<Config>>,
        incoming: Sender<IncomingMessage<Config>>,
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
        let (read, write) = match stream.into_split().await {
            Ok(split) => split,
            Err(err) => {
                log::error!("({:?}) Couldn't split stream: {}", id, err);
                self.ecs_connection.disconnect_task.cancel();
                let _ = incoming
                    .send(IncomingMessage::Closed(ConnectionClosed::new(
                        ReceiveError::Io(err),
                        peer_addr,
                        Some(id),
                    )))
                    .await;
                return;
            }
        };
        let transport = self.ecs_connection.transport().clone();
        let (send_error_tx, send_error_rx) = tokio::sync::oneshot::channel();
        crate::runtime::spawn(Self::receive_packets(
            read,
            self.ecs_connection,
            ReceiveTaskState {
                codecs: PacketCodecs {
                    serializer: Arc::clone(&serializer),
                    packet_length_serializer: Arc::clone(&packet_length_serializer),
                },
                receive_limits,
                send_error: send_error_rx,
            },
            datagram_packets,
            incoming,
            peer_addr,
        ));
        crate::runtime::spawn(send_packets(
            write,
            PacketCodecs {
                serializer,
                packet_length_serializer,
            },
            SendTaskState {
                packets_rx,
                disconnect_task,
                id,
                transport,
                send_error: send_error_tx,
            },
        ));
    }

    async fn receive_packets(
        mut read: impl PacketReader,
        ecs_conn: ClientConnection<Config>,
        state: ReceiveTaskState<ClientSerializer<Config>, Config::LengthSerializer>,
        datagram_packets: LossySender<PacketReceiveEvent<Config>>,
        incoming: Sender<IncomingMessage<Config>>,
        peer_addr: SocketAddr,
    ) {
        let ReceiveTaskState {
            codecs:
                PacketCodecs {
                    serializer,
                    packet_length_serializer,
                },
            receive_limits,
            mut send_error,
        } = state;
        let disconnect_task = &ecs_conn.disconnect_task;
        let id = ecs_conn.id();
        let _guard = disconnect_task.clone().drop_guard();
        let datagram_packets = PacketForwarder::new(
            datagram_packets,
            Config::Protocol::DATAGRAM,
            disconnect_task.clone(),
            ecs_conn.transport().clone(),
        );
        let error = loop {
            tokio::select! {
                biased;
                Ok(error) = &mut send_error => break ReceiveError::Io(error),
                () = disconnect_task.cancelled() => break ReceiveError::IntentionalDisconnection,
                result = read.receive_with_timestamp(Arc::clone(&serializer), &*packet_length_serializer, &receive_limits) => {
                    match result {
                        Ok((packet, received_at)) => {
                            log::trace!("({id:?}) Received packet {packet:?}");
                            let event = PacketReceiveEvent::<Config> {
                                connection: ecs_conn.clone(),
                                packet,
                                received_at,
                            };
                            let forwarded = if Config::Protocol::DATAGRAM {
                                datagram_packets.forward(event, |discarded| {
                                    discarded.connection.record_drop(QueueDropReason::ReceiveQueueEvicted);
                                }).await
                            } else {
                                tokio::select! {
                                    biased;
                                    () = disconnect_task.cancelled() => false,
                                    result = incoming.send(IncomingMessage::StreamPacket(event)) => result.is_ok(),
                                }
                            };
                            if !forwarded {
                                break ReceiveError::IntentionalDisconnection;
                            }
                        }
                        Err(err) => break err,
                    }
                }
            }
        };
        // A failed writer cancels forwarding too; retain its cause even if the
        // receive task was waiting for ECS queue capacity when cancellation arrived.
        let error = send_error.try_recv().map_or(error, ReceiveError::Io);
        disconnect_task.cancel();
        read.close();
        if let Err(err) = incoming
            .send(IncomingMessage::Closed(ConnectionClosed::<Config>::new(
                error,
                peer_addr,
                Some(id),
            )))
            .await
        {
            log::debug!("({id:?}) Disconnection receiver closed: {err:?}");
        }
    }
}

impl<Config: ClientConfig> ConnectionRequestSender<Config> {
    fn observe(
        connection_request: On<ConnectionRequestEvent<Config>>,
        requests: Res<Self>,
        mut commands: Commands,
    ) {
        let address = connection_request.event().address;
        if let Err(err) = requests.0.try_send(address) {
            let kind = match err {
                tokio::sync::mpsc::error::TrySendError::Full(_) => io::ErrorKind::WouldBlock,
                tokio::sync::mpsc::error::TrySendError::Closed(_) => io::ErrorKind::BrokenPipe,
            };
            commands.trigger(
                ConnectionClosed::<Config>::new(
                    ReceiveError::NoConnection(io::Error::new(kind, "connection request rejected")),
                    address,
                    None,
                )
                .event,
            );
        }
    }
}

fn receive_datagram_packets<Config: ClientConfig>(
    mut datagram_packets: ResMut<DatagramPacketReceiver<Config>>,
    queues: Option<Res<NetworkQueueSettings>>,
    settings: Option<Res<ClientSettings<Config>>>,
    mut commands: Commands,
) {
    let budget = ClientSettings::<Config>::resolve_queues(settings.as_deref(), queues.as_deref())
        .events_per_frame;
    for _ in 0..budget {
        let next = datagram_packets.receiver.try_recv_if(|packet| {
            packet.connection.is_published() || packet.connection.disconnect_task.is_cancelled()
        });
        let Ok(packet) = next else { break };
        if packet.connection.disconnect_task.is_cancelled() {
            packet
                .connection
                .record_drop(QueueDropReason::ClosedBeforeDelivery);
        } else {
            commands.queue(move |world: &mut World| {
                if packet.connection.disconnect_task.is_cancelled() {
                    packet
                        .connection
                        .record_drop(QueueDropReason::ClosedBeforeDelivery);
                } else {
                    world.trigger(packet);
                }
            });
        }
    }
}

fn incoming_system<Config: ClientConfig>(
    mut incoming: ResMut<IncomingReceiver<Config>>,
    queues: Option<Res<NetworkQueueSettings>>,
    settings: Option<Res<ClientSettings<Config>>>,
    mut commands: Commands,
) {
    let budget = ClientSettings::<Config>::resolve_queues(settings.as_deref(), queues.as_deref())
        .events_per_frame;
    for _ in 0..budget {
        match incoming.0.try_recv() {
            Ok(IncomingMessage::Established(event)) => {
                // Registry changes and observers share the same command order as packets.
                commands.queue(move |world: &mut World| {
                    world.insert_resource(event.connection.clone());
                    world
                        .resource_mut::<ClientConnections<Config>>()
                        .register(event.connection.clone());
                    event.connection.mark_published();
                    world.trigger(event);
                });
            }
            Ok(IncomingMessage::StreamPacket(packet)) => commands.trigger(packet),
            Ok(IncomingMessage::Closed(closed)) => {
                commands.queue(move |world: &mut World| {
                    if let Some(id) = closed.id {
                        world.remove_resource::<ClientConnection<Config>>();
                        let fallback = world
                            .resource_mut::<ClientConnections<Config>>()
                            .remove_connection(id);
                        if let Some(connection) = fallback {
                            world.insert_resource(connection);
                        }
                    }
                    world.trigger(closed.event);
                });
            }
            Err(_) => break,
        }
    }
}

/// Indicates that the transport is ready.
/// For UDP this is local socket readiness, not a handshake or remote liveness check.
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
    pub error: ReceiveError<
        Config::DecodeError,
        <Config::LengthSerializer as PacketLengthSerializer>::Error,
    >,
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

#[cfg(all(test, feature = "protocol_udp", feature = "serializer_bitcode_serde"))]
mod udp_lifecycle_tests {
    use super::*;
    use crate::connection::lossy_channel;
    use crate::connection::OverflowPolicy;
    use crate::protocols::protocol::{FramedWriter, WriteStream};
    use crate::protocols::udp::{UdpConnectionHandle, UdpProtocol};
    use crate::serializers::bitcode_serde::BitcodeSerdeSerializer;
    use crate::serializers::packet_length_serializer::LittleEndian;
    use crate::serializers::serializer::SerializerAdapter;
    use tokio_util::sync::CancellationToken;

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
    fn startup_uses_endpoint_queue_capacity() {
        let mut app = App::new();
        app.insert_resource(NetworkQueueSettings {
            receive_capacity: 11,
            ..Default::default()
        });
        app.insert_resource(ClientSettings::<Config>::default().with_queues(
            NetworkQueueSettings {
                receive_capacity: 3,
                ..Default::default()
            },
        ));
        app.add_plugins(ClientPlugin::<Config>::new());
        app.update();
        assert_eq!(
            app.world()
                .resource::<ConnectionRequestSender<Config>>()
                .0
                .max_capacity(),
            3
        );
    }

    #[test]
    fn cancelled_udp_packet_is_dropped_while_close_waits_behind_establish() {
        let settings = NetworkQueueSettings {
            events_per_frame: 1,
            ..Default::default()
        };
        let (incoming_tx, incoming_rx) = settings.incoming_channel();
        let (packet_tx, packet_rx) = lossy_channel(1, usize::MAX, OverflowPolicy::DropNewest);
        let (outgoing, _rx) = settings.outgoing_channel::<_, UdpConnectionHandle>(true);
        let udp = UdpConnectionHandle::new(128, None);
        let address: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let connection = EcsConnection {
            _endpoint: PhantomData,
            disconnect_task: CancellationToken::new(),
            id: ConnectionId::next(),
            published: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            packet_tx: outgoing,
            transport: udp.clone(),
            local_addr: address,
            peer_addr: address,
        };
        assert!(incoming_tx
            .try_send(IncomingMessage::Established(ConnectionEstablishEvent {
                address,
                connection: connection.clone(),
            }))
            .is_ok());
        assert!(incoming_tx
            .try_send(IncomingMessage::Closed(ConnectionClosed::new(
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
        app.insert_resource(IncomingReceiver::<Config>(incoming_rx));
        app.insert_resource(DatagramPacketReceiver::<Config> {
            receiver: packet_rx,
        });
        app.insert_resource(PacketEvents::default());
        configure_receive_systems::<Config>(&mut app);
        app.add_observer(
            |_: On<PacketReceiveEvent<Config>>, mut events: ResMut<PacketEvents>| {
                events.0 += 1;
            },
        );

        app.insert_resource(ClientSettings::<Config>::default().with_queues(
            NetworkQueueSettings {
                events_per_frame: 0,
                ..settings
            },
        ));
        app.update();
        assert!(app
            .world()
            .resource::<ClientConnections<Config>>()
            .is_empty());
        assert_eq!(udp.stats().dropped_closed_before_delivery, 0);
        app.world_mut().remove_resource::<ClientSettings<Config>>();

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

    #[test]
    fn udp_observer_commands_are_visible_after_receive() {
        let settings = NetworkQueueSettings::default();
        let (incoming_tx, incoming_rx) = settings.incoming_channel();
        let (packet_tx, packet_rx) = lossy_channel(1, usize::MAX, OverflowPolicy::DropNewest);
        let (outgoing, _rx) = settings.outgoing_channel::<_, UdpConnectionHandle>(true);
        let address = ([127, 0, 0, 1], 1234).into();
        let connection = EcsConnection {
            _endpoint: PhantomData,
            disconnect_task: CancellationToken::new(),
            id: ConnectionId::next(),
            published: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            packet_tx: outgoing,
            transport: UdpConnectionHandle::new(128, None),
            local_addr: address,
            peer_addr: address,
        };
        assert!(incoming_tx
            .try_send(IncomingMessage::Established(ConnectionEstablishEvent {
                address,
                connection: connection.clone(),
            }))
            .is_ok());
        assert!(packet_tx
            .try_send(
                PacketReceiveEvent {
                    connection,
                    packet: 7,
                    received_at: Instant::now(),
                },
                1
            )
            .is_ok());
        let mut app = App::new();
        app.insert_resource(ClientConnections::<Config>::new())
            .insert_resource(IncomingReceiver::<Config>(incoming_rx))
            .insert_resource(DatagramPacketReceiver::<Config> {
                receiver: packet_rx,
            })
            .add_observer(
                |_: On<ConnectionEstablishEvent<Config>>, mut commands: Commands| {
                    commands.init_resource::<PacketEvents>();
                },
            )
            .add_observer(
                |event: On<PacketReceiveEvent<Config>>,
                 connections: Res<ClientConnections<Config>>,
                 events: Res<PacketEvents>,
                 mut commands: Commands| {
                    assert_eq!(connections.first().unwrap().id(), event.connection.id());
                    commands.insert_resource(PacketEvents(events.0 + 1));
                },
            )
            .add_systems(
                PreUpdate,
                (|events: Res<PacketEvents>| assert_eq!(events.0, 1))
                    .after(ClientSystems::<Config>::RECEIVE),
            )
            .add_systems(Update, |events: Res<PacketEvents>| assert_eq!(events.0, 1));
        configure_receive_systems::<Config>(&mut app);
        app.update();
        assert_eq!(app.world().resource::<PacketEvents>().0, 1);
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
            _endpoint: PhantomData::<ClientPlugin<Config>>,
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
        let task = tokio::spawn(send_packets(
            FramedWriter::new(PendingWrite(Arc::clone(&entered))),
            PacketCodecs {
                serializer: Arc::new(Config::build_serializer()),
                packet_length_serializer: Arc::new(LittleEndian::<u32>::default()),
            },
            SendTaskState {
                packets_rx,
                disconnect_task: connection.disconnect_task.clone(),
                id: connection.id(),
                transport: udp.clone(),
                send_error: tokio::sync::oneshot::channel().0,
            },
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
        let (incoming_tx, incoming_rx) = settings.incoming_channel();
        let (packet_tx, packet_rx) = lossy_channel(2, usize::MAX, OverflowPolicy::DropNewest);
        let (outgoing, _rx) = settings.outgoing_channel::<_, UdpConnectionHandle>(true);
        let udp = UdpConnectionHandle::new(128, None);
        let address: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let connection = EcsConnection {
            _endpoint: PhantomData,
            disconnect_task: CancellationToken::new(),
            id: ConnectionId::next(),
            published: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            packet_tx: outgoing,
            transport: udp.clone(),
            local_addr: address,
            peer_addr: address,
        };
        assert!(incoming_tx
            .try_send(IncomingMessage::Established(ConnectionEstablishEvent {
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
        app.insert_resource(IncomingReceiver::<Config>(incoming_rx));
        app.insert_resource(DatagramPacketReceiver::<Config> {
            receiver: packet_rx,
        });
        app.insert_resource(PacketEvents::default());
        configure_receive_systems::<Config>(&mut app);
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
    use crate::protocols::tcp::TcpProtocol;
    use crate::serializers::bitcode_serde::BitcodeSerdeSerializer;
    use crate::serializers::packet_length_serializer::LittleEndian;
    use crate::serializers::serializer::SerializerAdapter;
    use tokio_util::sync::CancellationToken;

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
    fn final_tcp_packet_precedes_registry_removal() {
        for budget in [2, 3] {
            assert_tcp_observer_command_order(budget);
        }
    }

    fn assert_tcp_observer_command_order(budget: usize) {
        #[derive(Resource)]
        struct Closed;

        let settings = NetworkQueueSettings {
            events_per_frame: budget,
            ..Default::default()
        };
        let (incoming_tx, incoming_rx) = settings.incoming_channel();
        let (outgoing, _rx) = settings.outgoing_channel::<_, ()>(false);
        let address = ([127, 0, 0, 1], 1234).into();
        let connection = EcsConnection {
            _endpoint: PhantomData,
            disconnect_task: CancellationToken::new(),
            id: ConnectionId::next(),
            published: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            packet_tx: outgoing,
            transport: (),
            local_addr: address,
            peer_addr: address,
        };
        assert!(incoming_tx
            .try_send(IncomingMessage::Established(ConnectionEstablishEvent {
                address,
                connection: connection.clone(),
            }))
            .is_ok());
        assert!(incoming_tx
            .try_send(IncomingMessage::StreamPacket(PacketReceiveEvent {
                connection: connection.clone(),
                packet: 9,
                received_at: Instant::now(),
            }))
            .is_ok());
        assert!(incoming_tx
            .try_send(IncomingMessage::Closed(ConnectionClosed::new(
                ReceiveError::IntentionalDisconnection,
                address,
                Some(connection.id()),
            )))
            .is_ok());
        connection.disconnect();

        let mut app = App::new();
        app.insert_resource(settings);
        app.insert_resource(ClientConnections::<Config>::new());
        app.insert_resource(IncomingReceiver::<Config>(incoming_rx));
        configure_receive_systems::<Config>(&mut app);
        app.add_observer(
            |_: On<ConnectionEstablishEvent<Config>>, mut commands: Commands| {
                commands.init_resource::<Packets>();
            },
        );
        app.add_observer(
            |event: On<PacketReceiveEvent<Config>>,
             packets: Res<Packets>,
             connections: Res<ClientConnections<Config>>,
             mut commands: Commands| {
                assert_eq!(connections.len(), 1);
                assert!(packets.0.is_empty());
                commands.insert_resource(Packets(vec![event.event().packet]));
            },
        );
        app.add_observer(
            |_: On<DisconnectionEvent<Config>>,
             packets: Res<Packets>,
             connections: Res<ClientConnections<Config>>,
             mut commands: Commands| {
                assert_eq!(packets.0, [9]);
                assert!(connections.is_empty());
                commands.insert_resource(Closed);
            },
        );
        app.add_systems(
            PreUpdate,
            (|packets: Res<Packets>,
              connections: Res<ClientConnections<Config>>,
              closed: Option<Res<Closed>>| {
                assert_eq!(packets.0, [9]);
                assert_eq!(closed.is_some(), connections.is_empty());
            })
            .after(ClientSystems::<Config>::RECEIVE),
        );
        app.update();
        assert_eq!(app.world().resource::<Packets>().0, [9]);
        assert_eq!(
            app.world().resource::<ClientConnections<Config>>().len(),
            usize::from(budget == 2)
        );
        app.update();
        assert!(app
            .world()
            .resource::<ClientConnections<Config>>()
            .is_empty());
        assert_eq!(app.world().resource::<Packets>().0, [9]);
        assert!(app.world().contains_resource::<Closed>());
    }

    #[derive(Default, Resource)]
    struct RequestFailures(Vec<(SocketAddr, io::ErrorKind)>);

    #[test]
    fn rejected_connection_requests_emit_failure_events() {
        let address: SocketAddr = "127.0.0.1:1234".parse().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(address).unwrap();
        let mut app = App::new();
        app.insert_resource(ConnectionRequestSender::<Config>(tx, PhantomData));
        app.init_resource::<RequestFailures>();
        app.add_observer(ConnectionRequestSender::<Config>::observe);
        app.add_observer(
            |event: On<DisconnectionEvent<Config>>, mut failures: ResMut<RequestFailures>| {
                assert!(event.connection_id.is_none());
                let ReceiveError::NoConnection(error) = &event.error else {
                    panic!("expected a failed connection attempt");
                };
                failures.0.push((event.address, error.kind()));
            },
        );
        app.world_mut()
            .trigger(ConnectionRequestEvent::<Config>::new(address));
        app.world_mut().flush();
        assert_eq!(
            app.world().resource::<RequestFailures>().0,
            [(address, io::ErrorKind::WouldBlock)]
        );
        assert_eq!(rx.try_recv().unwrap(), address);
        drop(rx);
        app.world_mut()
            .trigger(ConnectionRequestEvent::<Config>::new(address));
        app.world_mut().flush();
        assert_eq!(
            app.world().resource::<RequestFailures>().0,
            [
                (address, io::ErrorKind::WouldBlock),
                (address, io::ErrorKind::BrokenPipe),
            ]
        );
    }
}

#[cfg(feature = "bench-internals")]
#[doc(hidden)]
#[path = "../benches/utils/client.rs"]
pub mod bench_utils;
