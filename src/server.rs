//! Server part of the plugin. You can enable it by adding `server` feature.

use crate::connection::settings::{EndpointReceiveLimits, EndpointSetup};
use crate::connection::NetworkSettings;

/// Optional settings for this server config, overriding app-wide defaults.
pub type ServerSettings<Config> = NetworkSettings<ServerPlugin<Config>>;

/// Scheduling phases for one server config; see [`crate::NetworkSystems`].
///
/// ```
/// use bevy::prelude::*;
/// use bevy_slinet::{ServerConfig, server::ServerSystems};
///
/// fn configure<C: ServerConfig>(app: &mut App) {
///     app.add_systems(PreUpdate,
///         consume_network_state.after(ServerSystems::<C>::RECEIVE));
/// }
///
/// fn consume_network_state() { /* Read state populated by packet observers. */ }
/// ```
pub type ServerSystems<Config> = crate::NetworkSystems<ServerPlugin<Config>>;

use std::marker::PhantomData;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use bevy::platform::time::Instant;
use bevy::{log, prelude::*};
use tokio::select;
use tokio::sync::mpsc::{Receiver, Sender};

use crate::connection::tasks::{
    run_connections, send_packets, PacketCodecs, ReceiveTaskState, SendTaskState,
};
use crate::connection::{lossy_channel, LossyReceiver, LossySender};
use crate::connection::{
    ConnectionId, EcsConnection, NetworkQueueSettings, PacketForwarder, RawConnection,
    ReceiveLimits,
};
use crate::protocols::protocol::{
    Listener, NetworkStream, PacketReader, Protocol, QueueDropReason, ReceiveError,
};
use crate::serializers::serializer::Serializer;
use crate::{PacketLengthSerializer, ServerConfig, SystemSets};

/// Represents the server side of a client connection.
pub type ServerConnection<Config> = EcsConnection<
    <Config as ServerConfig>::ServerPacket,
    <<Config as ServerConfig>::Protocol as Protocol>::Handle,
    ServerPlugin<Config>,
>;
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

    fn register(&mut self, connection: ServerConnection<Config>) {
        self.0.push(connection);
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
        crate::scheduling::register_global_limits(app);
        crate::runtime::register(app);
        app.init_resource::<EndpointReceiveLimits<Self>>()
            .insert_resource(ServerConnections::<Config>::new())
            // Startup: settings -> setup; warnings inspect the synchronized settings.
            .configure_sets(
                Startup,
                (
                    ServerSystems::<Config>::SETTINGS.in_set(SystemSets::SetMaxPacketSize),
                    ServerSystems::<Config>::SETUP.after(ServerSystems::<Config>::SETTINGS),
                ),
            )
            .add_systems(
                Startup,
                (
                    ServerSettings::<Config>::sync_limits.in_set(ServerSystems::<Config>::SETTINGS),
                    ServerSettings::<Config>::warning_system
                        .after(ServerSystems::<Config>::SETTINGS)
                        .in_set(SystemSets::MaxPacketSizeWarning),
                    Self::setup_system(self.address).in_set(ServerSystems::<Config>::SETUP),
                ),
            )
            // Update: synchronize runtime limit changes for this endpoint.
            .configure_sets(
                Update,
                ServerSystems::<Config>::SETTINGS.in_set(SystemSets::SetMaxPacketSize),
            )
            .add_systems(
                Update,
                ServerSettings::<Config>::sync_limits.in_set(ServerSystems::<Config>::SETTINGS),
            );
        // PreUpdate: all incoming events, using the protocol-specific graph below.
        configure_receive_systems::<Config>(app);
        #[cfg(target_family = "wasm")]
        app.add_systems(
            PreUpdate,
            publish_server_address::<Config>.before(ServerSystems::<Config>::RECEIVE),
        );
        app.configure_sets(
            Startup,
            ServerSystems::<Config>::SETUP.after(crate::runtime::RuntimeSetup),
        );
    }
}

// This is also used by socket-free fixtures, so tests and benchmarks exercise
// the plugin's actual set hierarchy and deferred-command boundaries.
fn configure_receive_systems<Config: ServerConfig>(app: &mut App) {
    app.configure_sets(
        PreUpdate,
        (
            ServerSystems::<Config>::RECEIVE.in_set(SystemSets::ServerReceive),
            ServerSystems::<Config>::LIFECYCLE
                .in_set(ServerSystems::<Config>::RECEIVE)
                .in_set(SystemSets::ServerAcceptNewConnections)
                .in_set(SystemSets::ServerRemoveConnections),
            ServerSystems::<Config>::PACKETS
                .in_set(ServerSystems::<Config>::RECEIVE)
                .in_set(SystemSets::ServerAcceptNewPackets),
        ),
    );
    let incoming = incoming_system::<Config>.in_set(ServerSystems::<Config>::LIFECYCLE);
    if Config::Protocol::DATAGRAM {
        // Apply connection commands before looking for packets of published peers.
        // The dependency is local to this config, not every plugin of this role.
        app.configure_sets(
            PreUpdate,
            ServerSystems::<Config>::PACKETS.after(ServerSystems::<Config>::LIFECYCLE),
        )
        .add_systems(
            PreUpdate,
            (
                incoming,
                receive_datagram_packets::<Config>.in_set(ServerSystems::<Config>::PACKETS),
            ),
        );
    } else {
        // Streams keep establishment, packets and closure in one budgeted FIFO.
        app.add_systems(PreUpdate, incoming.in_set(ServerSystems::<Config>::PACKETS));
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
struct IncomingReceiver<Config: ServerConfig>(Receiver<IncomingMessage<Config>>);

enum IncomingMessage<Config: ServerConfig> {
    StreamPacket(PacketReceiveEvent<Config>),
    Established(NewConnectionEvent<Config>),
    Closed(DisconnectionEvent<Config>),
}

#[derive(Resource)]
struct DatagramPacketReceiver<Config: ServerConfig> {
    receiver: LossyReceiver<PacketReceiveEvent<Config>>,
}

// Browser bind completes asynchronously, so publish its result in a later frame.
#[cfg(target_family = "wasm")]
#[derive(Resource)]
struct PendingServerAddress<Config: ServerConfig> {
    receiver: std::sync::Mutex<std::sync::mpsc::Receiver<SocketAddr>>,
    _marker: PhantomData<Config>,
}

#[cfg(target_family = "wasm")]
fn publish_server_address<Config: ServerConfig>(
    pending: Option<Res<PendingServerAddress<Config>>>,
    mut commands: Commands,
) {
    let Some(pending) = pending else { return };
    let result = pending
        .receiver
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .try_recv();
    match result {
        Ok(address) => {
            commands.insert_resource(ServerAddress::<Config> {
                address,
                _marker: PhantomData,
            });
            commands.remove_resource::<PendingServerAddress<Config>>();
        }
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            commands.remove_resource::<PendingServerAddress<Config>>();
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => {}
    }
}

impl<Config: ServerConfig> ServerPlugin<Config> {
    fn setup_system(
        address: SocketAddr,
    ) -> impl Fn(Commands, EndpointSetup<Self>, Res<crate::runtime::NetworkRuntime>) {
        move |commands, endpoint, runtime| {
            Self::setup(
                commands,
                address,
                endpoint.queues(),
                endpoint.receive_limits(),
                &runtime,
            );
        }
    }

    fn setup(
        mut commands: Commands,
        address: SocketAddr,
        queues: NetworkQueueSettings,
        limits: ReceiveLimits,
        runtime: &crate::runtime::NetworkRuntime,
    ) {
        let (incoming_tx, incoming_rx) = queues.incoming_channel();
        let (connection_sender, incoming_connections) = queues.incoming_channel();
        let (datagram_packet_tx, datagram_packet_rx) = lossy_channel(
            queues.receive_capacity.max(1),
            usize::MAX,
            queues.datagram_receive_overflow,
        );
        let (disconnect_sender, incoming_disconnects) = queues.incoming_channel();
        commands.insert_resource(IncomingReceiver::<Config>(incoming_rx));
        if Config::Protocol::DATAGRAM {
            commands.insert_resource(DatagramPacketReceiver::<Config> {
                receiver: datagram_packet_rx,
            });
        }
        // Native startup waits for bind; browsers must return to their event loop.
        let (listener_address_tx, listener_address_rx) = std::sync::mpsc::sync_channel(1);

        runtime.spawn_local(move || async move {
            crate::runtime::spawn(Self::process_connections(
                incoming_connections,
                queues.receive_capacity,
                datagram_packet_tx,
                incoming_tx.clone(),
                disconnect_sender,
            ));
            Self::accept_connections(
                address,
                queues,
                limits,
                incoming_tx,
                connection_sender,
                incoming_disconnects,
                listener_address_tx,
            )
            .await;
        });

        #[cfg(target_family = "wasm")]
        commands.insert_resource(PendingServerAddress::<Config> {
            receiver: std::sync::Mutex::new(listener_address_rx),
            _marker: PhantomData,
        });
        // Native clients may connect right after Startup, so bind must finish first.
        #[cfg(not(target_family = "wasm"))]
        if let Ok(local_addr) = listener_address_rx.recv() {
            commands.insert_resource(ServerAddress::<Config> {
                address: local_addr,
                _marker: PhantomData,
            });
        }
    }

    async fn accept_connections(
        address: SocketAddr,
        queues: NetworkQueueSettings,
        limits: ReceiveLimits,
        incoming: Sender<IncomingMessage<Config>>,
        connection_sender: Sender<ConnectedTransport<Config>>,
        mut incoming_disconnects: Receiver<SocketAddr>,
        listener_address_tx: std::sync::mpsc::SyncSender<SocketAddr>,
    ) {
        let listener = match Config::Protocol::bind(address).await {
            Ok(listener) => listener,
            Err(err) => {
                log::error!("Couldn't create listener at {}: {}", address, err);
                return;
            }
        };
        let _ = listener_address_tx.send(listener.address());
        let mut warned = false;
        // Do not pause UDP acceptance: accept() also dispatches existing peers.
        // Reject excess new peers before allocating a serializer or spawning work.
        let pending = Arc::new(tokio::sync::Semaphore::new(queues.receive_capacity.max(1)));
        loop {
            select! {
                result = listener.accept() => {
                    let stream = match result {
                        Ok(stream) => stream,
                        Err(error) if matches!(
                            error.kind(),
                            std::io::ErrorKind::Interrupted
                                | std::io::ErrorKind::ConnectionAborted
                                | std::io::ErrorKind::ConnectionReset
                        ) => {
                            tokio::task::yield_now().await;
                            continue;
                        }
                        Err(error) => {
                            log::error!("Listener at {} failed to accept: {error}", listener.address());
                            break;
                        }
                    };
                    let Ok(permit) = Arc::clone(&pending).try_acquire_owned() else {
                        drop(stream);
                        continue;
                    };
                    log::debug!("Accepting a connection from {:?}", stream.peer_addr());
                    let serializer = Config::build_serializer();
                    serializer.warn_if_stateful_over_datagrams::<Config::Protocol>(&mut warned);
                    let limits = limits.clone();
                    let incoming = incoming.clone();
                    let connection_sender = connection_sender.clone();
                    crate::runtime::spawn(async move {
                        let _permit = permit;
                        ConnectedTransport::<Config>::establish(
                            stream,
                            Arc::new(serializer),
                            queues,
                            limits,
                            incoming,
                            connection_sender,
                        ).await;
                    });
                }
                Some(addr) = incoming_disconnects.recv() => {
                    listener.handle_disconnection(addr);
                }
                else => break,
            }
        }
    }

    async fn process_connections(
        incoming_connections: Receiver<ConnectedTransport<Config>>,
        setup_limit: usize,
        datagram_packets: LossySender<PacketReceiveEvent<Config>>,
        incoming: Sender<IncomingMessage<Config>>,
        disconnect_sender: Sender<SocketAddr>,
    ) {
        run_connections(incoming_connections, setup_limit, |connection| {
            connection.run(
                datagram_packets.clone(),
                incoming.clone(),
                disconnect_sender.clone(),
            )
        })
        .await;
    }
}

impl<Config: ServerConfig> ConnectedTransport<Config> {
    async fn establish(
        stream: <Config::Protocol as Protocol>::ServerStream,
        serializer: Arc<ServerSerializer<Config>>,
        queues: NetworkQueueSettings,
        limits: ReceiveLimits,
        incoming: Sender<IncomingMessage<Config>>,
        connection_sender: Sender<Self>,
    ) {
        let (tx, rx) = queues.outgoing_channel(Config::Protocol::DATAGRAM);
        let connection = RawConnection::with_limits(
            stream,
            serializer,
            Config::LengthSerializer::default(),
            rx,
            limits,
        );
        let ecs_conn = connection.ecs_connection(tx);
        if let Err(err) = incoming
            .send(IncomingMessage::Established(NewConnectionEvent::<Config> {
                address: ecs_conn.peer_addr,
                connection: ecs_conn.clone(),
            }))
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
        datagram_packets: LossySender<PacketReceiveEvent<Config>>,
        incoming: Sender<IncomingMessage<Config>>,
        disconnect_sender: Sender<SocketAddr>,
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
        let (read, write) = match stream.into_split().await {
            Ok(split) => split,
            Err(err) => {
                log::error!("({:?}) Couldn't split stream: {}", id, err);
                self.ecs_connection.disconnect_task.cancel();
                let _ = incoming
                    .send(IncomingMessage::Closed(DisconnectionEvent {
                        error: ReceiveError::Io(err),
                        connection: self.ecs_connection,
                    }))
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
            disconnect_sender,
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
        ecs_conn: ServerConnection<Config>,
        state: ReceiveTaskState<ServerSerializer<Config>, Config::LengthSerializer>,
        datagram_packets: LossySender<PacketReceiveEvent<Config>>,
        incoming: Sender<IncomingMessage<Config>>,
        disconnect_sender: Sender<SocketAddr>,
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
            .send(IncomingMessage::Closed(DisconnectionEvent::<Config> {
                error,
                connection: ecs_conn.clone(),
            }))
            .await
        {
            log::debug!("({id:?}) Disconnection receiver closed: {err:?}");
        }
        if let Err(err) = disconnect_sender.send(ecs_conn.peer_addr).await {
            log::debug!("({id:?}) Listener closed: {err}");
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

/// A transport peer is available.
/// For UDP the first datagram creates a local peer; its sender is not validated.
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
    pub error: ReceiveError<
        Config::DecodeError,
        <Config::LengthSerializer as PacketLengthSerializer>::Error,
    >,
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
    /// Custom protocols use [`PacketReader::receive_with_timestamp`] semantics.
    pub received_at: Instant,
}

fn incoming_system<Config: ServerConfig>(
    mut incoming: ResMut<IncomingReceiver<Config>>,
    queues: Option<Res<NetworkQueueSettings>>,
    settings: Option<Res<ServerSettings<Config>>>,
    mut commands: Commands,
) {
    let budget = ServerSettings::<Config>::resolve_queues(settings.as_deref(), queues.as_deref())
        .events_per_frame;
    for _ in 0..budget {
        match incoming.0.try_recv() {
            Ok(IncomingMessage::Established(event)) => {
                // Registry changes and observers share the same command order as packets.
                commands.queue(move |world: &mut World| {
                    world
                        .resource_mut::<ServerConnections<Config>>()
                        .register(event.connection.clone());
                    event.connection.mark_published();
                    world.trigger(event);
                });
            }
            Ok(IncomingMessage::StreamPacket(packet)) => commands.trigger(packet),
            Ok(IncomingMessage::Closed(closed)) => {
                commands.queue(move |world: &mut World| {
                    world
                        .resource_mut::<ServerConnections<Config>>()
                        .remove_connection(closed.connection.id());
                    world.trigger(closed);
                });
            }
            Err(_) => break,
        }
    }
}

fn receive_datagram_packets<Config: ServerConfig>(
    mut datagram_packets: ResMut<DatagramPacketReceiver<Config>>,
    queues: Option<Res<NetworkQueueSettings>>,
    settings: Option<Res<ServerSettings<Config>>>,
    mut commands: Commands,
) {
    let budget = ServerSettings::<Config>::resolve_queues(settings.as_deref(), queues.as_deref())
        .events_per_frame;
    if !Config::Protocol::DATAGRAM {
        return;
    }
    for _ in 0..budget {
        let next = datagram_packets.receiver.try_recv_if(|packet| {
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

#[cfg(all(test, feature = "protocol_tcp", feature = "serializer_bitcode_serde"))]
mod listener_tests {
    use super::*;
    use crate::protocols::tcp::TcpNetworkStream;
    use crate::serializers::bitcode_serde::BitcodeSerdeSerializer;
    use crate::serializers::packet_length_serializer::LittleEndian;
    use crate::serializers::serializer::SerializerAdapter;
    use std::io::{self, ErrorKind};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static ACCEPTS: AtomicUsize = AtomicUsize::new(0);

    struct ErrorListener(SocketAddr);

    #[async_trait::async_trait]
    impl Listener for ErrorListener {
        type Stream = TcpNetworkStream;

        async fn accept(&self) -> io::Result<Self::Stream> {
            let error = match ACCEPTS.fetch_add(1, Ordering::SeqCst) {
                0 => ErrorKind::Interrupted,
                1 => ErrorKind::ConnectionAborted,
                2 => ErrorKind::ConnectionReset,
                _ => ErrorKind::PermissionDenied,
            };
            Err(error.into())
        }

        fn address(&self) -> SocketAddr {
            self.0
        }
    }

    struct ErrorProtocol;

    #[async_trait::async_trait]
    impl Protocol for ErrorProtocol {
        type Handle = ();
        type Listener = ErrorListener;
        type ServerStream = TcpNetworkStream;
        type ClientStream = TcpNetworkStream;

        async fn bind(address: SocketAddr) -> io::Result<Self::Listener> {
            Ok(ErrorListener(address))
        }
    }

    struct Config;

    impl ServerConfig for Config {
        type ClientPacket = u8;
        type ServerPacket = u8;
        type Protocol = ErrorProtocol;
        type EncodeError = bitcode::Error;
        type DecodeError = bitcode::Error;
        type LengthSerializer = LittleEndian<u32>;

        fn build_serializer() -> SerializerAdapter<u8, u8, bitcode::Error, bitcode::Error> {
            SerializerAdapter::ReadOnly(Arc::new(BitcodeSerdeSerializer))
        }
    }

    #[tokio::test]
    async fn listener_retries_transient_errors_and_stops_on_fatal_error() {
        let queues = NetworkQueueSettings::default();
        let (incoming, _messages) = queues.incoming_channel();
        let (connections, _accepted) = queues.incoming_channel();
        // Keep the channel open and empty, as it is before the first connection.
        let (_disconnects, incoming_disconnects) = queues.incoming_channel();
        let (address_tx, _address_rx) = std::sync::mpsc::sync_channel(1);
        let task = ServerPlugin::<Config>::accept_connections(
            ([127, 0, 0, 1], 1234).into(),
            queues,
            ReceiveLimits::default(),
            incoming,
            connections,
            incoming_disconnects,
            address_tx,
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), task)
                .await
                .is_ok()
        );
        assert_eq!(ACCEPTS.load(Ordering::SeqCst), 4);
    }
}

#[cfg(all(test, feature = "protocol_udp", feature = "serializer_bitcode_serde"))]
mod udp_lifecycle_tests {
    use super::*;
    use crate::connection::lossy_channel;
    use crate::connection::OverflowPolicy;
    use crate::protocols::udp::{UdpConnectionHandle, UdpProtocol};
    use crate::serializers::bitcode_serde::BitcodeSerdeSerializer;
    use crate::serializers::packet_length_serializer::LittleEndian;
    use crate::serializers::serializer::SerializerAdapter;
    use tokio_util::sync::CancellationToken;

    struct Config;
    impl ServerConfig for Config {
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
    fn cancelled_udp_packet_is_dropped_while_server_close_waits() {
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
            .try_send(IncomingMessage::Established(NewConnectionEvent {
                address,
                connection: connection.clone(),
            }))
            .is_ok());
        assert!(incoming_tx
            .try_send(IncomingMessage::Closed(DisconnectionEvent {
                error: ReceiveError::IntentionalDisconnection,
                connection: connection.clone(),
            }))
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
        app.insert_resource(ServerConnections::<Config>::new());
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

        app.insert_resource(ServerSettings::<Config>::default().with_queues(
            NetworkQueueSettings {
                events_per_frame: 0,
                ..settings
            },
        ));
        app.update();
        assert!(app
            .world()
            .resource::<ServerConnections<Config>>()
            .is_empty());
        assert_eq!(udp.stats().dropped_closed_before_delivery, 0);
        app.world_mut().remove_resource::<ServerSettings<Config>>();

        app.update();
        assert_eq!(app.world().resource::<ServerConnections<Config>>().len(), 1);
        assert_eq!(app.world().resource::<PacketEvents>().0, 0);
        assert_eq!(udp.stats().dropped_closed_before_delivery, 1);
        app.update();
        assert!(app
            .world()
            .resource::<ServerConnections<Config>>()
            .is_empty());
        assert_eq!(app.world().resource::<PacketEvents>().0, 0);
    }
}

#[cfg(feature = "bench-internals")]
#[doc(hidden)]
#[path = "../benches/utils/server.rs"]
pub mod bench_utils;
