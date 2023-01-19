//! This module contains structs that are used connection handling.

use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use bevy::prelude::{Res, Resource};
use futures::task::AtomicWaker;
use tokio::sync::mpsc::error::SendError;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::packet_length_serializer::PacketLengthSerializer;
use crate::protocol::NetworkStream;
use crate::serializer::Serializer;

/// The ecs-side connection struct. There is 2 structs,
/// one raw (with the stream, runs on another thread),
/// and ecs that can be cheaply cloned and interacts with
/// the raw connection via [`tokio::sync::mpsc`].
#[derive(Resource)]
pub struct EcsConnection<SendingPacket>
where
    SendingPacket: Send + Sync + Debug + 'static,
{
    pub(crate) disconnect_task: DisconnectTask,
    pub(crate) id: ConnectionId,
    pub(crate) packet_tx: UnboundedSender<SendingPacket>,
    pub(crate) local_addr: SocketAddr,
    pub(crate) peer_addr: SocketAddr,
}

impl<SendingPacket> Clone for EcsConnection<SendingPacket>
where
    SendingPacket: Send + Sync + Debug + 'static,
{
    fn clone(&self) -> Self {
        EcsConnection {
            disconnect_task: self.disconnect_task.clone(),
            id: self.id,
            packet_tx: self.packet_tx.clone(),
            local_addr: self.local_addr,
            peer_addr: self.peer_addr,
        }
    }
}

impl<SendingPacket> Debug for EcsConnection<SendingPacket>
where
    SendingPacket: Send + Sync + Debug + 'static,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Connection #{}", self.id().0)
    }
}

impl<SendingPacket> EcsConnection<SendingPacket>
where
    SendingPacket: Send + Sync + Debug + 'static,
{
    /// Returns this connection's [`ID`](ConnectionId).
    pub fn id(&self) -> ConnectionId {
        self.id
    }

    /// Returns the socket address of the remote peer of this TCP connection.
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    /// Returns the socket address of the local half of this TCP connection.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Sends a packet to the server. Returns error if disconnected.
    pub fn send(&self, packet: SendingPacket) -> Result<(), SendError<SendingPacket>> {
        self.packet_tx.send(packet)
    }

    /// Closes the connection.
    pub fn disconnect(&self) {
        self.disconnect_task.disconnect();
    }
}

pub(crate) struct RawConnection<ReceivingPacket, SendingPacket, NS, S, LS>
where
    ReceivingPacket: Send + Sync + Debug + 'static,
    SendingPacket: Send + Sync + Debug + 'static,
    NS: NetworkStream,
    S: Serializer<ReceivingPacket, SendingPacket>,
    LS: PacketLengthSerializer,
{
    pub disconnect_task: DisconnectTask,
    pub stream: NS,
    pub serializer: Arc<S>,
    pub packet_length_serializer: Arc<LS>,
    pub packets_rx: UnboundedReceiver<SendingPacket>,
    pub id: ConnectionId,
    pub _receive_packet: PhantomData<ReceivingPacket>,
    pub _send_packet: PhantomData<SendingPacket>,
}

impl<ReceivingPacket, SendingPacket, NS, S, LS> Debug
    for RawConnection<ReceivingPacket, SendingPacket, NS, S, LS>
where
    ReceivingPacket: Send + Sync + Debug + 'static,
    SendingPacket: Send + Sync + Debug + 'static,
    NS: NetworkStream,
    S: Serializer<ReceivingPacket, SendingPacket>,
    LS: PacketLengthSerializer,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "RawConnection #{}", self.id().0)
    }
}

/// A connection ID is an unique connection identifier that is mainly used
/// in servers with multiple clients. This ID should only be used locally
/// and is not meant to be exposed to the other side or stored in a database.
/// Client-side ConnectionId and server-side ConnectionId are NOT the same.
/// ConnectionId is basically an static AtomicUsize counter, so it resets
/// every server restart. If there are multiple clients/servers running
/// (like in multiple_connections example), they'll have a single connection
/// counter that increments for every clientside/serverside connection.
#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, bevy::ecs::component::Component)]
pub struct ConnectionId(usize);

impl Debug for ConnectionId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

impl ConnectionId {
    /// Creates and returns a new, unique [`ConnectionId`].
    /// See the source code for implementation details.
    pub fn next() -> ConnectionId {
        static CONNECTION_ID: AtomicUsize = AtomicUsize::new(0);

        ConnectionId(CONNECTION_ID.fetch_add(1, Ordering::Relaxed))
    }
}

pub(crate) static MAX_PACKET_SIZE: AtomicUsize = AtomicUsize::new(usize::MAX);

/// We can't set it as a field in [`ClientConfig`](crate::ClientConfig) or [`ServerConfig`](crate::ServerConfig)
/// because using trait consts as const generics require `generic_const_exprs` feature. You should set
/// this resource to avoid out-of-memory attacks (where a client sends a packet with length-prefix of
/// 100000000000 bytes and bevy_slinet tries to allocate a buffer of that size).
#[derive(Copy, Clone, Resource)]
pub struct MaxPacketSize(pub usize);

impl<ReceivingPacket, SendingPacket, NS, S, LS>
    RawConnection<ReceivingPacket, SendingPacket, NS, S, LS>
where
    ReceivingPacket: Send + Sync + Debug + 'static,
    SendingPacket: Send + Sync + Debug + 'static,
    NS: NetworkStream,
    S: Serializer<ReceivingPacket, SendingPacket>,
    LS: PacketLengthSerializer,
{
    #[cfg(feature = "client")]
    pub fn new(
        stream: NS,
        serializer: S,
        packet_length_serializer: LS,
        packets_rx: UnboundedReceiver<SendingPacket>,
    ) -> Self {
        Self {
            disconnect_task: DisconnectTask::default(),
            stream,
            serializer: Arc::new(serializer),
            packet_length_serializer: Arc::new(packet_length_serializer),
            packets_rx,
            id: ConnectionId::next(),
            _receive_packet: PhantomData,
            _send_packet: PhantomData,
        }
    }

    pub fn id(&self) -> ConnectionId {
        self.id
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.stream.local_addr()
    }

    pub fn peer_addr(&self) -> SocketAddr {
        self.stream.peer_addr()
    }
}

#[derive(Default, Clone)]
pub(crate) struct DisconnectTask(Arc<DisconnectTaskInner>);

#[derive(Default)]
struct DisconnectTaskInner {
    disconnect: AtomicBool,
    waker: AtomicWaker,
}

impl DisconnectTask {
    fn disconnect(&self) {
        self.0.disconnect.store(true, Ordering::Relaxed);
        self.0.waker.wake();
    }
}

impl Future for DisconnectTask {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.0.disconnect.load(Ordering::Relaxed) {
            return Poll::Ready(());
        }

        self.0.waker.register(cx.waker());

        if self.0.disconnect.load(Ordering::Relaxed) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

pub(crate) fn set_max_packet_size_system(max_packet_size: Option<Res<MaxPacketSize>>) {
    match max_packet_size {
        Some(res) if res.is_changed() => {
            MAX_PACKET_SIZE.store(res.0, Ordering::Relaxed);
        }
        _ => (),
    }
}

pub(crate) fn max_packet_size_warning_system(max_packet_size: Option<Res<MaxPacketSize>>) {
    if max_packet_size.is_none() {
        log::warn!("You haven't set \"MaxPacketSize\" resource! This is a security risk, please insert it before using this in production.")
    }
}
