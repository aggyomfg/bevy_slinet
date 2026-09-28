//! Connects ECS packet queues to transport tasks.

use std::error::Error;
use std::fmt::{Debug, Formatter};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use bevy::prelude::Resource;
use tokio::sync::mpsc::Receiver;
use tokio_util::sync::CancellationToken;

#[cfg(any(feature = "client", feature = "server"))]
use crate::protocols::protocol::QueueDropReason;
use crate::protocols::protocol::{NetworkStream, TransportHandle};
use crate::serializers::packet_length_serializer::PacketLengthSerializer;
use crate::serializers::serializer::Serializer;

mod queue;
#[cfg(test)]
mod raw_tests;
mod send_error;
pub(crate) mod transport;

#[cfg(any(feature = "client", feature = "server"))]
pub(crate) use queue::PacketForwarder;
pub use queue::{NetworkQueueSettings, OutgoingReceiver, OutgoingSender, OverflowPolicy};
pub use send_error::SendError;

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
pub struct EcsConnection<SendingPacket, H: TransportHandle>
where
    SendingPacket: Send + Sync + Debug + 'static,
{
    pub(crate) disconnect_task: CancellationToken,
    pub(crate) id: ConnectionId,
    pub(crate) published: Arc<AtomicBool>,
    pub(crate) packet_tx: OutgoingSender<SendingPacket>,
    pub(crate) transport: H,
    pub(crate) local_addr: SocketAddr,
    pub(crate) peer_addr: SocketAddr,
}

impl<SendingPacket, H: TransportHandle> Clone for EcsConnection<SendingPacket, H>
where
    SendingPacket: Send + Sync + Debug + 'static,
{
    fn clone(&self) -> Self {
        Self {
            disconnect_task: self.disconnect_task.clone(),
            id: self.id,
            published: Arc::clone(&self.published),
            packet_tx: self.packet_tx.clone(),
            transport: self.transport.clone(),
            local_addr: self.local_addr,
            peer_addr: self.peer_addr,
        }
    }
}

impl<SendingPacket, H: TransportHandle> Debug for EcsConnection<SendingPacket, H>
where
    SendingPacket: Send + Sync + Debug + 'static,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Connection #{}", self.id().0)
    }
}

impl<SendingPacket, H: TransportHandle> EcsConnection<SendingPacket, H>
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
    pub fn send(&self, packet: SendingPacket) -> Result<(), SendError<SendingPacket>> {
        if self.disconnect_task.is_cancelled() {
            return Err(SendError::Closed(packet));
        }
        self.packet_tx.try_send(packet, &self.transport)
    }

    #[cfg(any(feature = "client", feature = "server"))]
    pub(crate) fn record_drop(&self, reason: QueueDropReason) {
        self.transport.record_drop(reason);
    }

    /// Returns this connection's typed transport controls and diagnostics.
    /// Clone the handle to retain access after the connection is dropped.
    #[must_use]
    pub const fn transport(&self) -> &H {
        &self.transport
    }

    /// Closes the local connection. UDP sends no notification to the remote peer.
    pub fn disconnect(&self) {
        self.disconnect_task.cancel();
    }
}

/// Owns the low-level transport and packet queue consumed by a connection task.
#[cfg_attr(
    not(any(feature = "client", feature = "server")),
    expect(
        dead_code,
        reason = "Endpoint tasks consume the private serializer and queue fields"
    )
)]
pub struct RawConnection<ReceivingPacket, SendingPacket, NS, EncErr, DecErr, LS>
where
    ReceivingPacket: Send + Sync + Debug + 'static,
    SendingPacket: Send + Sync + Debug + 'static,
    NS: NetworkStream,
    EncErr: Error + Send + Sync,
    DecErr: Error + Send + Sync,
    LS: PacketLengthSerializer,
{
    pub(crate) disconnect_task: CancellationToken,
    pub(crate) stream: NS,
    pub(crate) serializer: Arc<
        dyn Serializer<ReceivingPacket, SendingPacket, EncodeError = EncErr, DecodeError = DecErr>,
    >,
    pub(crate) packet_length_serializer: Arc<LS>,
    pub(crate) packets_rx: OutgoingReceiver<SendingPacket, NS::Handle>,
    pub(crate) receive_limits: ReceiveLimits,
    pub(crate) id: ConnectionId,
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
    /// Creates a connection with the default unlimited receive size.
    /// Use [`Self::receive_limits`] to configure a bound before starting reception.
    #[must_use]
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
        mut packets_rx: OutgoingReceiver<SendingPacket, NS::Handle>,
        receive_limits: ReceiveLimits,
    ) -> Self {
        packets_rx.set_transport(stream.transport());
        Self {
            disconnect_task: CancellationToken::default(),
            stream,
            serializer,
            packet_length_serializer: Arc::new(packet_length_serializer),
            packets_rx,
            receive_limits,
            id: ConnectionId::next(),
        }
    }

    #[cfg(any(feature = "client", feature = "server"))]
    pub(crate) fn ecs_connection(
        &self,
        packet_tx: OutgoingSender<SendingPacket>,
    ) -> EcsConnection<SendingPacket, NS::Handle> {
        EcsConnection {
            disconnect_task: self.disconnect_task.clone(),
            id: self.id,
            published: Arc::new(AtomicBool::new(false)),
            packet_tx,
            transport: self.stream.transport(),
            local_addr: self.local_addr(),
            peer_addr: self.peer_addr(),
        }
    }

    /// Borrows the underlying stream without replacing its transport state.
    #[must_use]
    pub const fn stream(&self) -> &NS {
        &self.stream
    }

    /// Takes ownership of the stream, dropping the outgoing queue and raw wrapper.
    #[must_use]
    pub fn into_stream(self) -> NS {
        self.stream
    }

    /// Returns the live receive limits used by this connection.
    #[must_use]
    pub const fn receive_limits(&self) -> &ReceiveLimits {
        &self.receive_limits
    }

    /// Clones this stream's shared transport controls and diagnostics.
    #[must_use]
    pub fn transport(&self) -> NS::Handle {
        self.stream.transport()
    }

    /// Signals cancellation to the tasks using this connection.
    pub fn disconnect(&self) {
        self.disconnect_task.cancel();
    }

    /// Identifies this connection independently of its peer address.
    #[must_use]
    pub const fn id(&self) -> ConnectionId {
        self.id
    }

    /// Returns the socket address of the local endpoint.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.stream.local_addr()
    }

    /// Returns the socket address of the remote peer.
    #[must_use]
    pub fn peer_addr(&self) -> SocketAddr {
        self.stream.peer_addr()
    }
}
