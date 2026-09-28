use std::{
    num::NonZeroU64,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::sync::watch;

/// A snapshot of UDP session activity. Byte counts include the 37-byte SLN2 header.
/// Successful socket sends do not imply that a peer received the datagram.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UdpStatsSnapshot {
    /// DATA datagrams accepted by the local socket.
    pub sent_data_packets: u64,
    /// Wire bytes in successfully sent DATA datagrams.
    pub sent_data_bytes: u64,
    /// Control datagrams accepted by the local socket.
    pub sent_control_packets: u64,
    /// Wire bytes in successfully sent control datagrams.
    pub sent_control_bytes: u64,
    /// DATA datagrams received for this session.
    pub received_data_packets: u64,
    /// Wire bytes in received DATA datagrams.
    pub received_data_bytes: u64,
    /// Control datagrams received for this session.
    pub received_control_packets: u64,
    /// Wire bytes in received control datagrams.
    pub received_control_bytes: u64,
    /// Outgoing datagrams rejected by a full queue.
    pub dropped_outgoing_queue_full: u64,
    /// Outgoing datagrams evicted to make room for newer ones.
    pub dropped_outgoing_queue_evicted: u64,
    /// Listener datagrams rejected by a full raw queue.
    pub dropped_raw_queue_full: u64,
    /// Listener datagrams evicted from the raw queue.
    pub dropped_raw_queue_evicted: u64,
    /// Decoded packets rejected by a full receive queue.
    pub dropped_receive_queue_full: u64,
    /// Decoded packets evicted from the receive queue.
    pub dropped_receive_queue_evicted: u64,
    /// Packets discarded when the session closed before delivery.
    pub dropped_closed_before_delivery: u64,
    /// Outgoing payloads exceeding the configured datagram size.
    pub dropped_oversized_payload: u64,
    /// Incoming payloads exceeding the app's receive limit.
    pub dropped_receive_limit: u64,
    /// Incoming payloads that could not be parsed or decoded.
    pub dropped_malformed_payload: u64,
    /// Outgoing DATA or control datagrams rejected by the socket.
    pub dropped_socket_send_error: u64,
}

#[derive(Clone, Copy, Debug)]
#[expect(
    clippy::redundant_pub_crate,
    reason = "Re-exported to ECS transport code"
)]
pub(crate) enum UdpDropReason {
    #[cfg(any(feature = "client", feature = "server"))]
    OutgoingQueueFull,
    #[cfg(any(feature = "client", feature = "server"))]
    OutgoingQueueEvicted,
    RawQueueFull,
    RawQueueEvicted,
    #[cfg(any(feature = "client", feature = "server"))]
    ReceiveQueueFull,
    #[cfg(any(feature = "client", feature = "server"))]
    ReceiveQueueEvicted,
    ClosedBeforeDelivery,
    OversizedPayload,
    ReceiveLimit,
    MalformedPayload,
    SocketSendError,
}

#[derive(Debug, Default)]
struct Counters {
    sent_data_packets: AtomicU64,
    sent_data_bytes: AtomicU64,
    sent_control_packets: AtomicU64,
    sent_control_bytes: AtomicU64,
    received_data_packets: AtomicU64,
    received_data_bytes: AtomicU64,
    received_control_packets: AtomicU64,
    received_control_bytes: AtomicU64,
    dropped_outgoing_queue_full: AtomicU64,
    dropped_outgoing_queue_evicted: AtomicU64,
    dropped_raw_queue_full: AtomicU64,
    dropped_raw_queue_evicted: AtomicU64,
    dropped_receive_queue_full: AtomicU64,
    dropped_receive_queue_evicted: AtomicU64,
    dropped_closed_before_delivery: AtomicU64,
    dropped_oversized_payload: AtomicU64,
    dropped_receive_limit: AtomicU64,
    dropped_malformed_payload: AtomicU64,
    dropped_socket_send_error: AtomicU64,
}

/// A clonable view of one UDP session's counters and DATA send rate.
/// It remains usable after the connection closes.
#[derive(Clone, Debug)]
pub struct UdpConnectionHandle {
    counters: Arc<Counters>,
    max_payload_size: usize,
    rate: watch::Sender<Option<NonZeroU64>>,
}

impl UdpConnectionHandle {
    pub(crate) fn new(max_payload_size: usize, rate: Option<NonZeroU64>) -> Self {
        Self {
            counters: Arc::default(),
            max_payload_size,
            rate: watch::channel(rate).0,
        }
    }

    /// Samples counters independently; fields may reflect concurrent activity at different instants.
    #[must_use]
    pub fn stats(&self) -> UdpStatsSnapshot {
        let c = &self.counters;
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        UdpStatsSnapshot {
            sent_data_packets: load(&c.sent_data_packets),
            sent_data_bytes: load(&c.sent_data_bytes),
            sent_control_packets: load(&c.sent_control_packets),
            sent_control_bytes: load(&c.sent_control_bytes),
            received_data_packets: load(&c.received_data_packets),
            received_data_bytes: load(&c.received_data_bytes),
            received_control_packets: load(&c.received_control_packets),
            received_control_bytes: load(&c.received_control_bytes),
            dropped_outgoing_queue_full: load(&c.dropped_outgoing_queue_full),
            dropped_outgoing_queue_evicted: load(&c.dropped_outgoing_queue_evicted),
            dropped_raw_queue_full: load(&c.dropped_raw_queue_full),
            dropped_raw_queue_evicted: load(&c.dropped_raw_queue_evicted),
            dropped_receive_queue_full: load(&c.dropped_receive_queue_full),
            dropped_receive_queue_evicted: load(&c.dropped_receive_queue_evicted),
            dropped_closed_before_delivery: load(&c.dropped_closed_before_delivery),
            dropped_oversized_payload: load(&c.dropped_oversized_payload),
            dropped_receive_limit: load(&c.dropped_receive_limit),
            dropped_malformed_payload: load(&c.dropped_malformed_payload),
            dropped_socket_send_error: load(&c.dropped_socket_send_error),
        }
    }

    /// Maximum application payload that fits in one configured DATA datagram.
    #[must_use]
    pub const fn max_payload_size(&self) -> usize {
        self.max_payload_size
    }

    /// Limits DATA wire bytes per second. `None` removes pacing immediately.
    pub fn set_send_rate(&self, bytes_per_second: Option<NonZeroU64>) {
        self.rate.send_replace(bytes_per_second);
    }

    pub(super) fn rate_updates(&self) -> watch::Receiver<Option<NonZeroU64>> {
        self.rate.subscribe()
    }

    pub(super) fn data_sent(&self, bytes: usize) {
        self.counters
            .sent_data_packets
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .sent_data_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub(super) fn control_sent(&self, bytes: usize) {
        self.counters
            .sent_control_packets
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .sent_control_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub(super) fn received(&self, bytes: usize, data: bool) {
        let (packets, wire_bytes) = if data {
            (
                &self.counters.received_data_packets,
                &self.counters.received_data_bytes,
            )
        } else {
            (
                &self.counters.received_control_packets,
                &self.counters.received_control_bytes,
            )
        };
        packets.fetch_add(1, Ordering::Relaxed);
        wire_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub(crate) fn count_drop(&self, reason: UdpDropReason) {
        let counter = match reason {
            #[cfg(any(feature = "client", feature = "server"))]
            UdpDropReason::OutgoingQueueFull => &self.counters.dropped_outgoing_queue_full,
            #[cfg(any(feature = "client", feature = "server"))]
            UdpDropReason::OutgoingQueueEvicted => &self.counters.dropped_outgoing_queue_evicted,
            UdpDropReason::RawQueueFull => &self.counters.dropped_raw_queue_full,
            UdpDropReason::RawQueueEvicted => &self.counters.dropped_raw_queue_evicted,
            #[cfg(any(feature = "client", feature = "server"))]
            UdpDropReason::ReceiveQueueFull => &self.counters.dropped_receive_queue_full,
            #[cfg(any(feature = "client", feature = "server"))]
            UdpDropReason::ReceiveQueueEvicted => &self.counters.dropped_receive_queue_evicted,
            UdpDropReason::ClosedBeforeDelivery => &self.counters.dropped_closed_before_delivery,
            UdpDropReason::OversizedPayload => &self.counters.dropped_oversized_payload,
            UdpDropReason::ReceiveLimit => &self.counters.dropped_receive_limit,
            UdpDropReason::MalformedPayload => &self.counters.dropped_malformed_payload,
            UdpDropReason::SocketSendError => &self.counters.dropped_socket_send_error,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}
