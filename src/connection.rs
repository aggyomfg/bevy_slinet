//! Connects ECS packet queues to transport tasks.

use std::error::Error;
use std::fmt::{Debug, Formatter};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use bevy::prelude::Resource;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{Receiver, Sender};

#[cfg(any(feature = "client", feature = "server"))]
use crate::packet_queue::{lossy_channel, LossyReceiver, LossySender};
#[cfg(feature = "protocol_udp")]
use crate::protocols::udp::UdpConnectionHandle;
#[cfg(all(feature = "protocol_udp", any(feature = "client", feature = "server")))]
use crate::protocols::udp::UdpDropReason;

use crate::protocols::protocol::NetworkStream;
use crate::serializers::packet_length_serializer::PacketLengthSerializer;
use crate::serializers::serializer::Serializer;

/// Chooses which queued datagram to discard when a UDP queue fills.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OverflowPolicy {
    /// Reject the newly submitted item.
    #[default]
    DropNewest,
    /// Evict the oldest queued items to admit the new item.
    DropOldest,
}

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
    #[cfg(feature = "protocol_udp")]
    pub(crate) udp: Option<UdpConnectionHandle>,
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
            #[cfg(feature = "protocol_udp")]
            udp: self.udp.clone(),
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
        #[cfg(all(feature = "protocol_udp", any(feature = "client", feature = "server")))]
        {
            self.packet_tx.try_send(packet, self.udp.as_ref())
        }
        #[cfg(not(all(feature = "protocol_udp", any(feature = "client", feature = "server"))))]
        {
            self.packet_tx.try_send(packet)
        }
    }

    /// Returns the UDP session handle, including counters retained after disconnect.
    #[cfg(feature = "protocol_udp")]
    #[must_use]
    pub fn udp(&self) -> Option<UdpConnectionHandle> {
        self.udp.clone()
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

/// Sending endpoint for a transport-specific outgoing packet queue.
pub struct OutgoingSender<T>(OutgoingSenderInner<T>);

#[cfg_attr(not(any(feature = "client", feature = "server")), allow(dead_code))]
enum OutgoingSenderInner<T> {
    Reliable(Sender<T>),
    #[cfg(any(feature = "client", feature = "server"))]
    Lossy(LossySender<T>),
}

impl<T> Clone for OutgoingSender<T> {
    fn clone(&self) -> Self {
        Self(match &self.0 {
            OutgoingSenderInner::Reliable(tx) => OutgoingSenderInner::Reliable(tx.clone()),
            #[cfg(any(feature = "client", feature = "server"))]
            OutgoingSenderInner::Lossy(tx) => OutgoingSenderInner::Lossy(tx.clone()),
        })
    }
}

impl<T> OutgoingSender<T> {
    #[cfg(any(feature = "client", feature = "server", test))]
    const fn reliable(tx: Sender<T>) -> Self {
        Self(OutgoingSenderInner::Reliable(tx))
    }

    #[cfg(any(feature = "client", feature = "server"))]
    const fn lossy(tx: LossySender<T>) -> Self {
        Self(OutgoingSenderInner::Lossy(tx))
    }

    fn try_send(
        &self,
        packet: T,
        #[cfg(all(feature = "protocol_udp", any(feature = "client", feature = "server")))]
        udp: Option<&UdpConnectionHandle>,
    ) -> Result<(), TrySendError<T>> {
        match &self.0 {
            OutgoingSenderInner::Reliable(tx) => tx.try_send(packet),
            #[cfg(any(feature = "client", feature = "server"))]
            OutgoingSenderInner::Lossy(tx) => match tx.try_send(packet, 1) {
                Ok(evicted) => {
                    #[cfg(feature = "protocol_udp")]
                    if let Some(udp) = udp {
                        for _ in evicted {
                            udp.count_drop(UdpDropReason::OutgoingQueueEvicted);
                        }
                    }
                    #[cfg(not(feature = "protocol_udp"))]
                    drop(evicted);
                    Ok(())
                }
                Err(err) => {
                    #[cfg(feature = "protocol_udp")]
                    if matches!(err, TrySendError::Full(_)) {
                        if let Some(udp) = udp {
                            udp.count_drop(UdpDropReason::OutgoingQueueFull);
                        }
                    }
                    Err(err)
                }
            },
        }
    }
}

/// Receiving endpoint for a transport-specific outgoing packet queue.
pub struct OutgoingReceiver<T> {
    inner: OutgoingReceiverInner<T>,
    #[cfg(all(feature = "protocol_udp", any(feature = "client", feature = "server")))]
    udp: Option<UdpConnectionHandle>,
}

#[cfg_attr(not(any(feature = "client", feature = "server")), allow(dead_code))]
enum OutgoingReceiverInner<T> {
    Reliable(Receiver<T>),
    #[cfg(any(feature = "client", feature = "server"))]
    Lossy(LossyReceiver<T>),
}

impl<T> From<Receiver<T>> for OutgoingReceiver<T> {
    fn from(receiver: Receiver<T>) -> Self {
        Self {
            inner: OutgoingReceiverInner::Reliable(receiver),
            #[cfg(all(feature = "protocol_udp", any(feature = "client", feature = "server")))]
            udp: None,
        }
    }
}

impl<T> OutgoingReceiver<T> {
    #[cfg(all(feature = "protocol_udp", any(feature = "client", feature = "server")))]
    pub(crate) fn set_udp_handle(&mut self, udp: Option<UdpConnectionHandle>) {
        self.udp = udp;
    }

    /// Receives the next packet or `None` when all senders have closed.
    pub async fn recv(&mut self) -> Option<T> {
        match &mut self.inner {
            OutgoingReceiverInner::Reliable(rx) => rx.recv().await,
            #[cfg(any(feature = "client", feature = "server"))]
            OutgoingReceiverInner::Lossy(rx) => rx.recv().await,
        }
    }
}

impl<T> Drop for OutgoingReceiver<T> {
    fn drop(&mut self) {
        #[cfg(all(feature = "protocol_udp", any(feature = "client", feature = "server")))]
        if let (Some(udp), OutgoingReceiverInner::Lossy(rx)) = (&self.udp, &mut self.inner) {
            let pending = rx.close_and_drain();
            for _ in &pending {
                udp.count_drop(UdpDropReason::ClosedBeforeDelivery);
            }
            drop(pending);
        }
    }
}

#[cfg(all(feature = "protocol_udp", any(feature = "client", feature = "server")))]
pub(crate) struct PendingUdpPacket(Option<UdpConnectionHandle>);

#[cfg(all(feature = "protocol_udp", any(feature = "client", feature = "server")))]
impl PendingUdpPacket {
    pub(crate) const fn new(handle: Option<UdpConnectionHandle>) -> Self {
        Self(handle)
    }

    pub(crate) fn disarm(&mut self) {
        self.0 = None;
    }
}

#[cfg(all(feature = "protocol_udp", any(feature = "client", feature = "server")))]
impl Drop for PendingUdpPacket {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.count_drop(UdpDropReason::ClosedBeforeDelivery);
        }
    }
}

/// Shared cancellation signal for all tasks belonging to a connection.
pub type DisconnectTask = tokio_util::sync::CancellationToken;

/// Limits the channels between ECS and network tasks. Insert before `Startup`.
///
/// Capacities count packets/events (not decoded bytes); zero capacities are clamped to one.
/// UDP applies the selected overflow policies; stream packets apply backpressure.
#[derive(Clone, Copy, Debug, Resource)]
pub struct NetworkQueueSettings {
    /// Outgoing packets per connection. UDP `DropNewest` reports `Full` on overflow.
    pub send_capacity: usize,
    /// Incoming packets and lifecycle events per plugin, each with its own bounded queue.
    pub receive_capacity: usize,
    /// Maximum items drained by each networking ECS system per frame. May change at runtime.
    pub events_per_frame: usize,
    /// Policy for a full UDP outgoing packet queue.
    pub udp_send_overflow: OverflowPolicy,
    /// Policy for a full UDP receive event queue.
    pub udp_receive_overflow: OverflowPolicy,
}

impl Default for NetworkQueueSettings {
    fn default() -> Self {
        Self {
            send_capacity: 1024,
            receive_capacity: 4096,
            events_per_frame: 256,
            udp_send_overflow: OverflowPolicy::DropNewest,
            udp_receive_overflow: OverflowPolicy::DropNewest,
        }
    }
}

#[cfg(any(feature = "client", feature = "server"))]
impl NetworkQueueSettings {
    pub(crate) fn outgoing_channel<T>(
        &self,
        datagram: bool,
    ) -> (OutgoingSender<T>, OutgoingReceiver<T>) {
        if datagram {
            let (tx, rx) = lossy_channel(
                self.send_capacity.max(1),
                usize::MAX,
                self.udp_send_overflow,
            );
            (
                OutgoingSender::lossy(tx),
                OutgoingReceiver {
                    inner: OutgoingReceiverInner::Lossy(rx),
                    #[cfg(all(
                        feature = "protocol_udp",
                        any(feature = "client", feature = "server")
                    ))]
                    udp: None,
                },
            )
        } else {
            let (tx, rx) = tokio::sync::mpsc::channel(self.send_capacity.max(1));
            (OutgoingSender::reliable(tx), OutgoingReceiver::from(rx))
        }
    }

    pub(crate) fn incoming_channel<T>(&self) -> (Sender<T>, Receiver<T>) {
        tokio::sync::mpsc::channel(self.receive_capacity.max(1))
    }
}

#[cfg(any(feature = "client", feature = "server"))]
pub(crate) struct PacketForwarder<T> {
    sender: LossySender<T>,
    datagram: bool,
    cancel: DisconnectTask,
    #[cfg(all(feature = "protocol_udp", any(feature = "client", feature = "server")))]
    udp: Option<UdpConnectionHandle>,
}
#[cfg(any(feature = "client", feature = "server"))]
impl<T> PacketForwarder<T> {
    pub(crate) const fn new(
        sender: LossySender<T>,
        datagram: bool,
        cancel: DisconnectTask,
        #[cfg(feature = "protocol_udp")] udp: Option<UdpConnectionHandle>,
    ) -> Self {
        Self {
            sender,
            datagram,
            cancel,
            #[cfg(feature = "protocol_udp")]
            udp,
        }
    }

    /// Returns false when forwarding must stop; UDP queue overflow drops the packet.
    pub(crate) async fn forward(&self, packet: T, on_evict: impl Fn(T)) -> bool {
        if self.datagram {
            match self.sender.try_send(packet, 1) {
                Ok(evicted) => {
                    for item in evicted {
                        on_evict(item);
                    }
                    true
                }
                Err(TrySendError::Closed(_)) => false,
                Err(TrySendError::Full(_)) => {
                    #[cfg(feature = "protocol_udp")]
                    if let Some(udp) = &self.udp {
                        udp.count_drop(UdpDropReason::ReceiveQueueFull);
                    }
                    true
                }
            }
        } else {
            tokio::select! {
                biased;
                () = self.cancel.cancelled() => false,
                result = self.sender.send(packet, 1) => result.is_ok(),
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
            published: Arc::new(AtomicBool::new(false)),
            packet_tx: OutgoingSender::reliable(packet_tx),
            #[cfg(feature = "protocol_udp")]
            udp: None,
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
        let (sender, mut receiver) = lossy_channel(1, usize::MAX, OverflowPolicy::DropNewest);
        let cancel = DisconnectTask::new();
        let udp = PacketForwarder::new(
            sender.clone(),
            true,
            cancel.clone(),
            #[cfg(feature = "protocol_udp")]
            None,
        );
        assert!(udp.forward(1, |_| {}).await);
        assert!(udp.forward(2, |_| {}).await);
        assert_eq!(receiver.recv().await, Some(1));
        sender.send(3, 1).await.unwrap();
        let tcp = PacketForwarder::new(
            sender,
            false,
            cancel.clone(),
            #[cfg(feature = "protocol_udp")]
            None,
        );
        let mut pending = Box::pin(tcp.forward(4, |_| {}));
        assert!(futures::poll!(pending.as_mut()).is_pending());
        cancel.cancel();
        assert!(!pending.await);
        assert_eq!(receiver.recv().await, Some(3));
    }

    #[cfg(all(feature = "protocol_udp", any(feature = "client", feature = "server")))]
    #[tokio::test]
    async fn udp_outgoing_policies_report_rejections_and_evictions() {
        let queues = NetworkQueueSettings {
            send_capacity: 1,
            udp_send_overflow: OverflowPolicy::DropNewest,
            ..Default::default()
        };
        let (packet_tx, mut receiver) = queues.outgoing_channel(true);
        let udp = UdpConnectionHandle::new(100, None);
        let connection = EcsConnection {
            disconnect_task: DisconnectTask::new(),
            id: ConnectionId::next(),
            published: Arc::new(AtomicBool::new(false)),
            packet_tx,
            udp: Some(udp.clone()),
            local_addr: "127.0.0.1:1".parse().unwrap(),
            peer_addr: "127.0.0.1:2".parse().unwrap(),
        };
        connection.send(1).unwrap();
        assert!(matches!(connection.send(2), Err(TrySendError::Full(2))));
        assert_eq!(receiver.recv().await, Some(1));
        assert_eq!(udp.stats().dropped_outgoing_queue_full, 1);

        let queues = NetworkQueueSettings {
            udp_send_overflow: OverflowPolicy::DropOldest,
            ..queues
        };
        let (packet_tx, mut receiver) = queues.outgoing_channel(true);
        let connection = EcsConnection {
            packet_tx,
            ..connection
        };
        connection.send(3).unwrap();
        connection.send(4).unwrap();
        assert_eq!(receiver.recv().await, Some(4));
        assert_eq!(udp.stats().dropped_outgoing_queue_evicted, 1);
        connection.disconnect();
        assert_eq!(
            connection
                .udp()
                .unwrap()
                .stats()
                .dropped_outgoing_queue_evicted,
            1
        );
    }

    #[cfg(all(feature = "protocol_udp", any(feature = "client", feature = "server")))]
    #[tokio::test]
    async fn udp_incoming_eviction_is_charged_to_discarded_session() {
        struct Packet {
            value: u8,
            udp: UdpConnectionHandle,
        }
        let older = UdpConnectionHandle::new(100, None);
        let newer = UdpConnectionHandle::new(100, None);
        let (tx, mut rx) = lossy_channel(1, usize::MAX, OverflowPolicy::DropOldest);
        let forwarder = PacketForwarder::new(tx, true, DisconnectTask::new(), Some(newer.clone()));
        assert!(
            forwarder
                .forward(
                    Packet {
                        value: 1,
                        udp: older.clone()
                    },
                    |discarded| {
                        discarded.udp.count_drop(UdpDropReason::ReceiveQueueEvicted);
                    }
                )
                .await
        );
        assert!(
            forwarder
                .forward(
                    Packet {
                        value: 2,
                        udp: newer.clone()
                    },
                    |discarded| {
                        discarded.udp.count_drop(UdpDropReason::ReceiveQueueEvicted);
                    }
                )
                .await
        );
        assert_eq!(rx.recv().await.map(|packet| packet.value), Some(2));
        assert_eq!(older.stats().dropped_receive_queue_evicted, 1);
        assert_eq!(newer.stats().dropped_receive_queue_evicted, 0);
    }
}
