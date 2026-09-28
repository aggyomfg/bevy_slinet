//! Connects ECS packet queues to transport tasks.

use std::error::Error;
use std::fmt::{Debug, Formatter};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use bevy::prelude::Resource;
use tokio::sync::mpsc::error::TrySendError;
#[cfg(feature = "client")]
use tokio::sync::mpsc::Receiver;

use crate::protocols::protocol::NetworkStream;
use crate::serializers::packet_length_serializer::PacketLengthSerializer;
use crate::serializers::serializer::Serializer;

mod queue;
pub(crate) mod transport;

#[cfg(any(feature = "client", feature = "server"))]
pub(crate) use queue::PacketForwarder;
pub use queue::{NetworkQueueSettings, OutgoingReceiver, OutgoingSender, OverflowPolicy};

use transport::ConnectionDiagnostics;
#[cfg(any(feature = "client", feature = "server"))]
use transport::QueueDropReason;

/// A live packet size limit shared by receive tasks in one Bevy app.
#[derive(Clone, Debug, Resource)]
pub struct ReceiveLimits(Arc<AtomicUsize>);

impl Default for ReceiveLimits {
    fn default() -> Self {
        Self::new(usize::MAX)
    }
}

impl ReceiveLimits {
    /// Creates a size limit in serialized payload bytes.
    #[must_use]
    pub fn new(max_packet_size: usize) -> Self {
        Self(Arc::new(AtomicUsize::new(max_packet_size)))
    }

    /// Returns the current maximum serialized payload size.
    #[must_use]
    pub fn max_packet_size(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }

    /// Changes the limit for all tasks using clones of this handle.
    pub fn set_max_packet_size(&self, max_packet_size: usize) {
        self.0.store(max_packet_size, Ordering::Relaxed);
    }
}

/// Provides a cloneable ECS handle to a transport task through a bounded packet queue.
#[derive(Resource)]
pub struct EcsConnection<SendingPacket>
where
    SendingPacket: Send + Sync + Debug + 'static,
{
    pub(crate) disconnect_task: DisconnectTask,
    pub(crate) id: ConnectionId,
    pub(crate) published: Arc<AtomicBool>,
    pub(crate) packet_tx: OutgoingSender<SendingPacket>,
    pub(crate) diagnostics: ConnectionDiagnostics,
    pub(crate) local_addr: SocketAddr,
    pub(crate) peer_addr: SocketAddr,
}

impl<SendingPacket> Clone for EcsConnection<SendingPacket>
where
    SendingPacket: Send + Sync + Debug + 'static,
{
    fn clone(&self) -> Self {
        Self {
            disconnect_task: self.disconnect_task.clone(),
            id: self.id,
            published: Arc::clone(&self.published),
            packet_tx: self.packet_tx.clone(),
            diagnostics: self.diagnostics.clone(),
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
    /// Identifies this connection independently of its peer address.
    #[must_use]
    pub const fn id(&self) -> ConnectionId {
        self.id
    }

    #[cfg(any(feature = "client", feature = "server"))]
    pub(crate) fn mark_published(&self) {
        self.published.store(true, Ordering::Release);
    }

    #[cfg(any(feature = "client", feature = "server"))]
    pub(crate) fn is_published(&self) -> bool {
        self.published.load(Ordering::Acquire)
    }

    /// Returns the socket address of the remote peer.
    #[must_use]
    pub const fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    /// Returns the socket address of the local endpoint.
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Queues a packet for the remote peer.
    ///
    /// # Errors
    /// Returns the unsent packet if the connection is closed or its outgoing queue is full.
    pub fn send(&self, packet: SendingPacket) -> Result<(), TrySendError<SendingPacket>> {
        if self.disconnect_task.is_cancelled() {
            return Err(TrySendError::Closed(packet));
        }
        self.packet_tx.try_send(packet, &self.diagnostics)
    }

    #[cfg(any(feature = "client", feature = "server"))]
    pub(crate) fn record_drop(&self, reason: QueueDropReason) {
        self.diagnostics.record_drop(reason);
    }

    /// Returns the UDP session handle, including counters retained after disconnect.
    #[cfg(feature = "protocol_udp")]
    #[must_use]
    pub fn udp(&self) -> Option<crate::protocols::udp::UdpConnectionHandle> {
        self.diagnostics.udp()
    }

    /// Closes the connection.
    pub fn disconnect(&self) {
        self.disconnect_task.cancel();
    }
}

pub struct RawConnection<ReceivingPacket, SendingPacket, NS, EncErr, DecErr, LS>
where
    ReceivingPacket: Send + Sync + Debug + 'static,
    SendingPacket: Send + Sync + Debug + 'static,
    NS: NetworkStream,
    EncErr: Error + Send + Sync,
    DecErr: Error + Send + Sync,
    LS: PacketLengthSerializer,
{
    pub disconnect_task: DisconnectTask,
    pub stream: NS,
    pub serializer: Arc<
        dyn Serializer<ReceivingPacket, SendingPacket, EncodeError = EncErr, DecodeError = DecErr>,
    >,
    pub packet_length_serializer: Arc<LS>,
    pub packets_rx: OutgoingReceiver<SendingPacket>,
    pub receive_limits: ReceiveLimits,
    pub id: ConnectionId,
}

impl<ReceivingPacket, SendingPacket, NS, EncErr, DecErr, LS> Debug
    for RawConnection<ReceivingPacket, SendingPacket, NS, EncErr, DecErr, LS>
where
    ReceivingPacket: Send + Sync + Debug + 'static,
    SendingPacket: Send + Sync + Debug + 'static,
    NS: NetworkStream,
    EncErr: Error + Send + Sync,
    DecErr: Error + Send + Sync,
    LS: PacketLengthSerializer,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "RawConnection #{}", self.id().0)
    }
}

/// Identifies a connection locally within this process; it is not a wire or persistent ID.
/// Client and server endpoints allocate independent IDs from the process-wide counter.
#[derive(Clone, bevy::ecs::component::Component, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ConnectionId(usize);
impl Debug for ConnectionId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

impl ConnectionId {
    /// Allocates the next ID from the process-wide counter.
    pub fn next() -> Self {
        static CONNECTION_ID: AtomicUsize = AtomicUsize::new(0);

        Self(CONNECTION_ID.fetch_add(1, Ordering::Relaxed))
    }
    /// Exposes the local counter value; it has no meaning to the remote peer.
    #[must_use]
    pub const fn read(&self) -> usize {
        self.0
    }
}

/// Limits incoming serialized payload sizes in bytes.
///
/// Networking plugins apply changes before network tasks start to an app-local limit.
/// Without a configured limit, stream peers can request arbitrarily large allocations.
#[derive(Clone, Copy, Resource)]
pub struct MaxPacketSize(pub usize);

#[cfg(any(feature = "client", feature = "server"))]
impl MaxPacketSize {
    pub(crate) fn set_system(
        max_packet_size: Option<bevy::prelude::Res<Self>>,
        limits: bevy::prelude::Res<ReceiveLimits>,
    ) {
        limits.set_max_packet_size(max_packet_size.map_or(usize::MAX, |res| res.0));
    }

    pub(crate) fn warning_system(max_packet_size: Option<bevy::prelude::Res<Self>>) {
        if max_packet_size.is_none() {
            bevy::log::warn!("You haven't set \"MaxPacketSize\" resource! This is a security risk, please insert it before using this in production.");
        }
    }
}

impl<ReceivingPacket, SendingPacket, NS, EncErr, DecErr, LS>
    RawConnection<ReceivingPacket, SendingPacket, NS, EncErr, DecErr, LS>
where
    ReceivingPacket: Send + Sync + Debug + 'static,
    SendingPacket: Send + Sync + Debug + 'static,
    NS: NetworkStream,
    EncErr: Error + Send + Sync,
    DecErr: Error + Send + Sync,
    LS: PacketLengthSerializer,
{
    /// Creates a client-side connection with the default unlimited receive size.
    #[cfg(feature = "client")]
    pub fn new(
        stream: NS,
        serializer: Arc<
            dyn Serializer<
                ReceivingPacket,
                SendingPacket,
                EncodeError = EncErr,
                DecodeError = DecErr,
            >,
        >,
        packet_length_serializer: LS,
        packets_rx: Receiver<SendingPacket>,
    ) -> Self {
        Self::with_limits(
            stream,
            serializer,
            packet_length_serializer,
            packets_rx.into(),
            ReceiveLimits::default(),
        )
    }

    #[cfg(feature = "client")]
    pub(crate) fn with_limits(
        stream: NS,
        serializer: Arc<
            dyn Serializer<
                ReceivingPacket,
                SendingPacket,
                EncodeError = EncErr,
                DecodeError = DecErr,
            >,
        >,
        packet_length_serializer: LS,
        packets_rx: OutgoingReceiver<SendingPacket>,
        receive_limits: ReceiveLimits,
    ) -> Self {
        Self {
            disconnect_task: DisconnectTask::default(),
            stream,
            serializer,
            packet_length_serializer: Arc::new(packet_length_serializer),
            packets_rx,
            receive_limits,
            id: ConnectionId::next(),
        }
    }

    pub const fn id(&self) -> ConnectionId {
        self.id
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.stream.local_addr()
    }

    pub fn peer_addr(&self) -> SocketAddr {
        self.stream.peer_addr()
    }
}

/// Shared cancellation signal for all tasks belonging to a connection.
pub type DisconnectTask = tokio_util::sync::CancellationToken;
