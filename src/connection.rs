//! Connects ECS packet queues to transport tasks.

use std::error::Error;
use std::fmt::{Debug, Formatter};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use bevy::prelude::Resource;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{Receiver, Sender};

use crate::packet_length_serializer::PacketLengthSerializer;
use crate::protocol::NetworkStream;
use crate::serializer::Serializer;

/// Provides a cloneable ECS handle to a transport task through a bounded packet queue.
#[derive(Resource)]
pub struct EcsConnection<SendingPacket>
where
    SendingPacket: Send + Sync + Debug + 'static,
{
    pub(crate) disconnect_task: DisconnectTask,
    pub(crate) id: ConnectionId,
    pub(crate) packet_tx: Sender<SendingPacket>,
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
    /// Identifies this connection independently of its peer address.
    pub fn id(&self) -> ConnectionId {
        self.id
    }

    /// Returns the socket address of the remote peer.
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    /// Returns the socket address of the local endpoint.
    pub fn local_addr(&self) -> SocketAddr {
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
        self.packet_tx.try_send(packet)
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
    pub packets_rx: Receiver<SendingPacket>,
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
    pub fn next() -> ConnectionId {
        static CONNECTION_ID: AtomicUsize = AtomicUsize::new(0);

        ConnectionId(CONNECTION_ID.fetch_add(1, Ordering::Relaxed))
    }
    /// Exposes the local counter value; it has no meaning to the remote peer.
    pub fn read(&self) -> usize {
        self.0
    }
}

pub(crate) static MAX_PACKET_SIZE: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Limits incoming serialized payload sizes in bytes.
/// Networking plugins apply changes during `Update` to a process-wide limit shared by all apps.
/// Without a configured limit, stream peers can request arbitrarily large allocations.
#[derive(Clone, Copy, Resource)]
pub struct MaxPacketSize(pub usize);

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
        Self {
            disconnect_task: DisconnectTask::default(),
            stream,
            serializer,
            packet_length_serializer: Arc::new(packet_length_serializer),
            packets_rx,
            id: ConnectionId::next(),
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

/// Shared cancellation signal for all tasks belonging to a connection.
pub type DisconnectTask = tokio_util::sync::CancellationToken;

#[cfg(any(feature = "client", feature = "server"))]
pub(crate) fn set_max_packet_size_system(
    max_packet_size: Option<bevy::prelude::Res<MaxPacketSize>>,
) {
    use bevy::prelude::DetectChanges;
    match max_packet_size {
        Some(res) if res.is_changed() => {
            MAX_PACKET_SIZE.store(res.0, Ordering::Relaxed);
        }
        _ => (),
    }
}

#[cfg(any(feature = "client", feature = "server"))]
pub(crate) fn max_packet_size_warning_system(
    max_packet_size: Option<bevy::prelude::Res<MaxPacketSize>>,
) {
    if max_packet_size.is_none() {
        bevy::log::warn!("You haven't set \"MaxPacketSize\" resource! This is a security risk, please insert it before using this in production.")
    }
}

/// Limits the channels between ECS and network tasks. Insert before `Startup`.
/// Capacities count packets/events (not decoded bytes); zero capacities are clamped to one.
/// UDP drops newly received packets when the ECS queue is full; streams apply backpressure.
#[derive(Clone, Copy, Debug, Resource)]
pub struct NetworkQueueSettings {
    /// Outgoing packets per connection. [`EcsConnection::send`] reports `Full` on overflow.
    pub send_capacity: usize,
    /// Incoming packets per plugin and capacity of each connection/event channel.
    pub receive_capacity: usize,
    /// Maximum items drained by each networking ECS system per frame. May change at runtime.
    pub events_per_frame: usize,
}

impl Default for NetworkQueueSettings {
    fn default() -> Self {
        Self {
            send_capacity: 1024,
            receive_capacity: 4096,
            events_per_frame: 256,
        }
    }
}

#[cfg(any(feature = "client", feature = "server"))]
impl NetworkQueueSettings {
    pub(crate) fn outgoing_channel<T>(&self) -> (Sender<T>, Receiver<T>) {
        tokio::sync::mpsc::channel(self.send_capacity.max(1))
    }

    pub(crate) fn incoming_channel<T>(&self) -> (Sender<T>, Receiver<T>) {
        tokio::sync::mpsc::channel(self.receive_capacity.max(1))
    }
}

#[cfg(any(feature = "client", feature = "server"))]
pub(crate) struct PacketForwarder<T> {
    sender: Sender<T>,
    datagram: bool,
    cancel: DisconnectTask,
}
#[cfg(any(feature = "client", feature = "server"))]
impl<T> PacketForwarder<T> {
    pub(crate) fn new(sender: Sender<T>, datagram: bool, cancel: DisconnectTask) -> Self {
        Self {
            sender,
            datagram,
            cancel,
        }
    }
    /// Returns false when forwarding must stop; a full UDP queue drops the packet.
    pub(crate) async fn forward(&self, packet: T) -> bool {
        if self.datagram {
            !matches!(self.sender.try_send(packet), Err(TrySendError::Closed(_)))
        } else {
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => false,
                result = self.sender.send(packet) => result.is_ok(),
            }
        }
    }
}

#[cfg(test)]
mod queue_tests {
    use super::*;

    #[test]
    fn outgoing_queue_reports_full_and_closed() {
        let (packet_tx, _receiver) = tokio::sync::mpsc::channel(1);
        let connection = EcsConnection {
            disconnect_task: DisconnectTask::new(),
            id: ConnectionId::next(),
            packet_tx,
            local_addr: "127.0.0.1:1".parse().unwrap(),
            peer_addr: "127.0.0.1:2".parse().unwrap(),
        };
        connection.send(1).unwrap();
        assert!(matches!(connection.send(2), Err(TrySendError::Full(2))));
        connection.disconnect();
        assert!(matches!(connection.send(3), Err(TrySendError::Closed(3))));
    }

    #[cfg(any(feature = "client", feature = "server"))]
    #[tokio::test]
    async fn udp_overflow_drops_new_packets_and_tcp_waits_cancel_safely() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let cancel = DisconnectTask::new();
        let udp = PacketForwarder::new(sender.clone(), true, cancel.clone());
        assert!(udp.forward(1).await);
        assert!(udp.forward(2).await);
        assert_eq!(receiver.recv().await, Some(1));
        assert!(receiver.try_recv().is_err());
        sender.send(3).await.unwrap();
        let tcp = PacketForwarder::new(sender, false, cancel.clone());
        let mut pending = Box::pin(tcp.forward(4));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        cancel.cancel();
        assert!(!pending.await);
        assert_eq!(receiver.recv().await, Some(3));
        assert!(receiver.try_recv().is_err());
    }
}
