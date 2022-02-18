//! Server part of the plugin. You can enable it by adding `server` feature.

use core::default::Default;
use std::io;
use std::marker::PhantomData;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;

use bevy::prelude::*;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::connection::{
    max_packet_size_system, max_packet_size_warning_system, ConnectionId, DisconnectTask,
    EcsConnection, RawConnection,
};
use crate::protocol::{Listener, NetworkStream, Protocol, ReadStream, ReceiveError, WriteStream};
use crate::{ServerConfig, SystemLabels};

/// Server-side connection to a server.
pub type ServerConnection<Config> = EcsConnection<<Config as ServerConfig>::ServerPacket>;

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
            .insert_resource(Vec::<ServerConnection<Config>>::new())
            .add_startup_system(create_setup_system::<Config>(self.address))
            .add_startup_system(
                max_packet_size_warning_system.label(SystemLabels::MaxPacketSizeWarning),
            )
            .add_system_to_stage(
                CoreStage::PostUpdate,
                max_packet_size_system.label(SystemLabels::SetMaxPacketSize),
            )
            .add_system_to_stage(
                CoreStage::PreUpdate,
                accept_new_connections::<Config>.label(SystemLabels::ServerAcceptNewConnections),
            )
            .add_system_to_stage(
                CoreStage::PreUpdate,
                accept_new_packets::<Config>.label(SystemLabels::ServerAcceptNewPackets),
            )
            .add_system_to_stage(
                CoreStage::PreUpdate,
                remove_connections::<Config>.label(SystemLabels::ServerRemoveConnections),
            )
            .add_system(connection_add_system::<Config>.label(SystemLabels::ServerConnectionAdd));
    }
}

impl<Config: ServerConfig> ServerPlugin<Config> {
    /// Bind to the specified address and return a [`ServerPlugin`].
    pub fn bind<A>(address: A) -> io::Result<ServerPlugin<Config>>
    where
        A: ToSocketAddrs,
    {
        Ok(ServerPlugin {
            address: address
                .to_socket_addrs()
                .expect("Invalid address")
                .next()
                .expect("Invalid address"),
            _marker: PhantomData,
        })
    }
}

struct ConnectionReceiver<Config: ServerConfig>(
    UnboundedReceiver<(SocketAddr, ServerConnection<Config>)>,
);
struct DisconnectionReceiver<Config: ServerConfig>(
    UnboundedReceiver<(
        ReceiveError<
            Config::ClientPacket,
            Config::ServerPacket,
            Config::Serializer,
            Config::LengthSerializer,
        >,
        ServerConnection<Config>,
    )>,
);
struct PacketReceiver<Config: ServerConfig>(
    UnboundedReceiver<(ServerConnection<Config>, Config::ClientPacket)>,
);

fn create_setup_system<Config: ServerConfig>(address: SocketAddr) -> impl Fn(Commands) {
    #[cfg(target_family = "wasm")]
    compile_error!("Why would you run a bevy_slinet server on WASM? If you really need this, please open an issue (https://github.com/Sliman4/bevy_slinet/issues/new)");

    move |mut commands: Commands| {
        let (conn_tx, conn_rx) = tokio::sync::mpsc::unbounded_channel();
        let (conn_tx2, mut conn_rx2): (
            UnboundedSender<(
                RawConnection<
                    Config::ClientPacket,
                    Config::ServerPacket,
                    <Config::Protocol as Protocol>::ServerStream,
                    Config::Serializer,
                    Config::LengthSerializer,
                >,
                ServerConnection<Config>,
            )>,
            UnboundedReceiver<(
                RawConnection<
                    Config::ClientPacket,
                    Config::ServerPacket,
                    <Config::Protocol as Protocol>::ServerStream,
                    Config::Serializer,
                    Config::LengthSerializer,
                >,
                ServerConnection<Config>,
            )>,
        ) = tokio::sync::mpsc::unbounded_channel();
        let (disc_tx, disc_rx) = tokio::sync::mpsc::unbounded_channel();
        let (pack_tx, pack_rx) = tokio::sync::mpsc::unbounded_channel();
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
                                id: _,
                                _receive_packet,
                                _send_packet,
                            } = connection;
                            let (mut read, mut write) =
                                stream.into_split().await.expect("Couldn't split stream");
                            let pack_tx2 = pack_tx.clone();
                            let disc_tx2 = disc_tx.clone();
                            let serializer2 = Arc::clone(&serializer);
                            let packet_length_serializer2 = Arc::clone(&packet_length_serializer);
                            tokio::spawn(async move {
                                loop {
                                    match read
                                        .receive(&*serializer2, &*packet_length_serializer2)
                                        .await
                                    {
                                        Ok(packet) => {
                                            log::trace!("Received packet {:?}", packet);
                                            pack_tx2.send((ecs_conn.clone(), packet)).unwrap();
                                        }
                                        Err(err) => {
                                            disc_tx2.send((err, ecs_conn.clone())).unwrap();
                                            break;
                                        }
                                    };
                                }
                            });
                            tokio::spawn(async move {
                                while let Some(packet) = packets_rx.recv().await {
                                    log::trace!("Sending packet {:?}", packet);
                                    match write
                                        .send(packet, &*serializer, &*packet_length_serializer)
                                        .await
                                    {
                                        Ok(()) => (),
                                        Err(err) => {
                                            log::error!("Error sending packet: {err}");
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

                    while let Ok(connection) = listener.accept().await {
                        log::debug!("Accepting a connection from {:?}", connection.peer_addr());
                        let (conn_tx_2, conn_tx2_2) = (conn_tx.clone(), conn_tx2.clone());
                        tokio::spawn(async move {
                            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                            let disconnect_task = DisconnectTask::default();
                            let connection = RawConnection {
                                disconnect_task: disconnect_task.clone(),
                                stream: connection,
                                serializer: Arc::new(Default::default()),
                                packet_length_serializer: Arc::new(Default::default()),
                                id: ConnectionId::next(),
                                packets_rx: rx,
                                _receive_packet: PhantomData,
                                _send_packet: PhantomData,
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
                });
        });
    }
}

/// A new client has connected.
pub struct NewConnectionEvent<Config: ServerConfig> {
    /// The connection.
    pub connection: ServerConnection<Config>,
    /// A client's IP address.
    pub address: SocketAddr,
}

/// A client disconnected.
pub struct DisconnectionEvent<Config: ServerConfig> {
    /// The error.
    pub error: ReceiveError<
        Config::ClientPacket,
        Config::ServerPacket,
        Config::Serializer,
        Config::LengthSerializer,
    >,
    /// The connection.
    pub connection: ServerConnection<Config>,
}

/// Sent for every packet received.
pub struct PacketReceiveEvent<Config: ServerConfig> {
    /// The connection.
    pub connection: ServerConnection<Config>,
    /// The packet.
    pub packet: Config::ClientPacket,
}

fn accept_new_connections<Config: ServerConfig>(
    mut receiver: ResMut<ConnectionReceiver<Config>>,
    mut event_writer: EventWriter<NewConnectionEvent<Config>>,
) {
    while let Ok((address, connection)) = receiver.0.try_recv() {
        event_writer.send(NewConnectionEvent {
            connection,
            address,
        })
    }
}

fn accept_new_packets<Config: ServerConfig>(
    mut receiver: ResMut<PacketReceiver<Config>>,
    mut event_writer: EventWriter<PacketReceiveEvent<Config>>,
) {
    while let Ok((connection, packet)) = receiver.0.try_recv() {
        event_writer.send(PacketReceiveEvent { connection, packet })
    }
}

fn remove_connections<Config: ServerConfig>(
    mut connections: ResMut<Vec<ServerConnection<Config>>>,
    mut disconnections: ResMut<DisconnectionReceiver<Config>>,
    mut event_writer: EventWriter<DisconnectionEvent<Config>>,
) {
    while let Ok((error, connection)) = disconnections.0.try_recv() {
        let index = connections
            .iter()
            .position(|conn| conn.id() == connection.id())
            .unwrap();
        connections.remove(index);

        event_writer.send(DisconnectionEvent { error, connection });
    }
}

fn connection_add_system<Config: ServerConfig>(
    mut connections: ResMut<Vec<ServerConnection<Config>>>,
    mut events: EventReader<NewConnectionEvent<Config>>,
) {
    for event in events.iter() {
        connections.push(event.connection.clone());
    }
}
