//! Server part of the plugin. You can enable it by adding `server` feature.

use std::future::Future;
use std::marker::PhantomData;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use bevy::platform::time::Instant;
use bevy::{log, prelude::*};
use tokio::select;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::CancellationToken;

use crate::connection::transport::{LifecycleQueue, PendingPacket};
use crate::connection::{
    ConnectionId, EcsConnection, MaxPacketSize, NetworkQueueSettings, OutgoingReceiver,
    PacketForwarder, RawConnection, ReceiveLimits,
};
use crate::packet_queue::{lossy_channel, LossyReceiver, LossySender};
use crate::protocols::protocol::{
    Listener, NetworkStream, PacketReader, PacketWriter, Protocol, QueueDropReason, ReceiveError,
};
use crate::serializers::serializer::Serializer;
use crate::{ServerConfig, SystemSets};

/// Represents the server side of a client connection.
pub type ServerConnection<Config> = EcsConnection<
    <Config as ServerConfig>::ServerPacket,
    <<Config as ServerConfig>::Protocol as Protocol>::Handle,
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
        app.init_resource::<ReceiveLimits>()
            .insert_resource(ServerConnections::<Config>::new())
            .add_systems(
                Startup,
                (
                    Self::setup_system(self.address).after(SystemSets::SetMaxPacketSize),
                    MaxPacketSize::warning_system.in_set(SystemSets::MaxPacketSizeWarning),
                ),
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
                (
                    lifecycle_system::<Config>
                        .in_set(SystemSets::ServerAcceptNewConnections)
                        .in_set(SystemSets::ServerRemoveConnections),
                    accept_new_packets::<Config>
                        .in_set(SystemSets::ServerAcceptNewPackets)
                        .after(SystemSets::ServerAcceptNewConnections),
                ),
            );
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
struct LifecycleReceiver<Config: ServerConfig>(LifecycleQueue<ServerLifecycle<Config>>);

enum ServerLifecycle<Config: ServerConfig> {
    Packet(PacketReceiveEvent<Config>),
    Established(NewConnectionEvent<Config>),
    Closed(DisconnectionEvent<Config>),
}

#[derive(Resource)]
struct PacketReceiver<Config: ServerConfig> {
    receiver: LossyReceiver<PacketReceiveEvent<Config>>,
}

impl<Config: ServerConfig> ServerPlugin<Config> {
    fn setup_system(
        address: SocketAddr,
    ) -> impl Fn(Commands, Option<Res<NetworkQueueSettings>>, Res<ReceiveLimits>) {
        #[cfg(target_family = "wasm")]
        compile_error!("Why would you run a bevy_slinet server on WASM? If you really need this, please open an issue (https://github.com/aggyomfg/bevy_slinet/issues/new)");

        move |commands, queues, limits: Res<ReceiveLimits>| {
            Self::setup(
                commands,
                address,
                queues.as_deref().copied().unwrap_or_default(),
                limits.clone(),
            );
        }
    }

    fn setup(
        mut commands: Commands,
        address: SocketAddr,
        queues: NetworkQueueSettings,
        limits: ReceiveLimits,
    ) {
        let (lifecycle_tx, lifecycle_rx) = queues.incoming_channel();
        let (connection_sender, incoming_connections) = queues.incoming_channel();
        let (pack_tx, pack_rx) = lossy_channel(
            queues.receive_capacity.max(1),
            usize::MAX,
            queues.datagram_receive_overflow,
        );
        let (disconnect_sender, incoming_disconnects) = queues.incoming_channel();
        commands.insert_resource(LifecycleReceiver::<Config>(lifecycle_rx.into()));
        commands.insert_resource(PacketReceiver::<Config> { receiver: pack_rx });
        let (bound_tx, bound_rx) = std::sync::mpsc::sync_channel(1);

        Self::run_async(move || async move {
            tokio::spawn(Self::process_connections(
                incoming_connections,
                pack_tx,
                lifecycle_tx.clone(),
                disconnect_sender,
            ));
            Self::accept_connections(
                address,
                queues,
                limits,
                lifecycle_tx,
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
        limits: ReceiveLimits,
        lifecycle: Sender<ServerLifecycle<Config>>,
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
                        limits.clone(),
                        lifecycle.clone(),
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
        packets: LossySender<PacketReceiveEvent<Config>>,
        lifecycle: Sender<ServerLifecycle<Config>>,
        disconnect_sender: Sender<SocketAddr>,
    ) {
        while let Some(connection) = incoming_connections.recv().await {
            connection
                .run(
                    packets.clone(),
                    lifecycle.clone(),
                    disconnect_sender.clone(),
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
        limits: ReceiveLimits,
        lifecycle: Sender<ServerLifecycle<Config>>,
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
        if let Err(err) = lifecycle
            .send(ServerLifecycle::Established(NewConnectionEvent::<Config> {
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
        packets: LossySender<PacketReceiveEvent<Config>>,
        lifecycle: Sender<ServerLifecycle<Config>>,
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
                let _ = lifecycle
                    .send(ServerLifecycle::Closed(DisconnectionEvent {
                        error: ReceiveError::Io(err),
                        connection: self.ecs_connection,
                    }))
                    .await;
                return;
            }
        };
        let transport = self.ecs_connection.transport().clone();
        let (send_error_tx, send_error_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(Self::receive_packets(
            read,
            self.ecs_connection,
            Arc::clone(&serializer),
            Arc::clone(&packet_length_serializer),
            packets,
            lifecycle,
            disconnect_sender,
            receive_limits,
            send_error_rx,
        ));
        tokio::spawn(Self::send_packets(
            write,
            packets_rx,
            serializer,
            packet_length_serializer,
            disconnect_task,
            id,
            transport,
            send_error_tx,
        ));
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Receive task needs transport, queue and lifecycle endpoints"
    )]
    async fn receive_packets(
        mut read: impl PacketReader,
        ecs_conn: ServerConnection<Config>,
        serializer: Arc<ServerSerializer<Config>>,
        packet_length_serializer: Arc<Config::LengthSerializer>,
        packets: LossySender<PacketReceiveEvent<Config>>,
        lifecycle: Sender<ServerLifecycle<Config>>,
        disconnect_sender: Sender<SocketAddr>,
        receive_limits: ReceiveLimits,
        mut send_error: tokio::sync::oneshot::Receiver<std::io::Error>,
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
                                packets.forward(event, |discarded| {
                                    discarded.connection.record_drop(QueueDropReason::ReceiveQueueEvicted);
                                }).await
                            } else {
                                tokio::select! {
                                    biased;
                                    () = disconnect_task.cancelled() => false,
                                    result = lifecycle.send(ServerLifecycle::Packet(event)) => result.is_ok(),
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
        if let Err(err) = lifecycle
            .send(ServerLifecycle::Closed(DisconnectionEvent::<Config> {
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

    #[expect(
        clippy::too_many_arguments,
        reason = "Send task owns transport, codec, cancellation and error reporting"
    )]
    async fn send_packets(
        mut write: impl PacketWriter,
        mut packets_rx: OutgoingReceiver<
            Config::ServerPacket,
            <Config::Protocol as Protocol>::Handle,
        >,
        serializer: Arc<ServerSerializer<Config>>,
        packet_length_serializer: Arc<Config::LengthSerializer>,
        disconnect_task: CancellationToken,
        id: ConnectionId,
        transport: <Config::Protocol as Protocol>::Handle,
        send_error: tokio::sync::oneshot::Sender<std::io::Error>,
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
                    let _ = send_error.send(err);
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
    /// Custom protocols use [`PacketReader::receive_with_timestamp`] semantics.
    pub received_at: Instant,
}

fn lifecycle_system<Config: ServerConfig>(
    mut lifecycle: ResMut<LifecycleReceiver<Config>>,
    mut connections: ResMut<ServerConnections<Config>>,
    queues: Option<Res<NetworkQueueSettings>>,
    mut commands: Commands,
) {
    let budget = queues
        .as_deref()
        .copied()
        .unwrap_or_default()
        .events_per_frame;
    for _ in 0..budget {
        match lifecycle
            .0
            .try_recv_if(|event| !matches!(event, ServerLifecycle::Packet(_)))
        {
            Some(ServerLifecycle::Established(event)) => {
                event.connection.mark_published();
                connections.register(event.connection.clone());
                commands.trigger(event);
            }
            Some(ServerLifecycle::Closed(event)) => {
                connections.remove_connection(event.connection.id());
                commands.trigger(event);
            }
            Some(ServerLifecycle::Packet(_)) | None => break,
        }
    }
}

fn accept_new_packets<Config: ServerConfig>(
    mut packets: ResMut<PacketReceiver<Config>>,
    mut lifecycle: ResMut<LifecycleReceiver<Config>>,
    queues: Option<Res<NetworkQueueSettings>>,
    mut commands: Commands,
) {
    let budget = queues
        .as_deref()
        .copied()
        .unwrap_or_default()
        .events_per_frame;
    if !Config::Protocol::DATAGRAM {
        for _ in 0..budget {
            let Some(ServerLifecycle::Packet(packet)) = lifecycle
                .0
                .try_recv_if(|event| matches!(event, ServerLifecycle::Packet(_)))
            else {
                break;
            };
            commands.trigger(packet);
        }
        return;
    }
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

#[cfg(all(test, feature = "protocol_udp", feature = "serializer_bitcode_serde"))]
mod udp_lifecycle_tests {
    use super::*;
    use crate::connection::OverflowPolicy;
    use crate::packet_queue::lossy_channel;
    use crate::protocols::udp::{UdpConnectionHandle, UdpProtocol};
    use crate::serializers::bitcode_serde::BitcodeSerdeSerializer;
    use crate::serializers::packet_length_serializer::LittleEndian;
    use crate::serializers::serializer::SerializerAdapter;

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
            .try_send(ServerLifecycle::Established(NewConnectionEvent {
                address,
                connection: connection.clone(),
            }))
            .is_ok());
        assert!(lifecycle_tx
            .try_send(ServerLifecycle::Closed(DisconnectionEvent {
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
        app.insert_resource(LifecycleReceiver::<Config>(lifecycle_rx.into()));
        app.insert_resource(PacketReceiver::<Config> {
            receiver: packet_rx,
        });
        app.insert_resource(PacketEvents::default());
        app.add_systems(
            PreUpdate,
            (
                lifecycle_system::<Config>.in_set(SystemSets::ServerAcceptNewConnections),
                accept_new_packets::<Config>.after(SystemSets::ServerAcceptNewConnections),
            ),
        );
        app.add_observer(
            |_: On<PacketReceiveEvent<Config>>, mut events: ResMut<PacketEvents>| {
                events.0 += 1;
            },
        );

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
