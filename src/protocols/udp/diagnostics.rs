use crate::protocols::protocol::{QueueDropReason, TransportHandle};
use std::{
    num::NonZeroU64,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::sync::watch;

/// A snapshot of UDP peer activity. Byte counts contain only serialized application bytes, excluding UDP/IP headers.
/// Successful socket sends do not imply that a peer received the datagram.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UdpStatsSnapshot {
    /// datagrams accepted by the local socket.
    pub sent_data_packets: u64,
    /// Wire bytes in successfully sent datagrams.
    pub sent_data_bytes: u64,
    /// datagrams received for this peer.
    pub received_data_packets: u64,
    /// Wire bytes in received datagrams.
    pub received_data_bytes: u64,
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
    /// Packets discarded when the peer closed before delivery.
    pub dropped_closed_before_delivery: u64,
    /// Outgoing payloads exceeding the configured datagram size.
    pub dropped_oversized_payload: u64,
    /// Incoming payloads exceeding the app's receive limit.
    pub dropped_receive_limit: u64,
    /// Incoming payloads that could not be parsed or decoded.
    pub dropped_malformed_payload: u64,
    /// Outgoing application datagrams rejected by the socket.
    pub dropped_socket_send_error: u64,
}

#[derive(Clone, Copy, Debug)]
pub(super) enum UdpDropReason {
    OutgoingQueueFull,
    OutgoingQueueEvicted,
    RawQueueFull,
    RawQueueEvicted,
    ReceiveQueueFull,
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
    received_data_packets: AtomicU64,
    received_data_bytes: AtomicU64,
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

/// A clonable view of one UDP peer's counters and datagram send rate.
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
            received_data_packets: load(&c.received_data_packets),
            received_data_bytes: load(&c.received_data_bytes),
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

    /// Maximum application payload that fits in one configured datagram.
    #[must_use]
    pub const fn max_payload_size(&self) -> usize {
        self.max_payload_size
    }

    /// Limits serialized bytes per second. `None` removes pacing immediately.
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

    pub(super) fn received(&self, bytes: usize) {
        self.counters
            .received_data_packets
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .received_data_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub(super) fn count_drop(&self, reason: UdpDropReason) {
        let counter = match reason {
            UdpDropReason::OutgoingQueueFull => &self.counters.dropped_outgoing_queue_full,
            UdpDropReason::OutgoingQueueEvicted => &self.counters.dropped_outgoing_queue_evicted,
            UdpDropReason::RawQueueFull => &self.counters.dropped_raw_queue_full,
            UdpDropReason::RawQueueEvicted => &self.counters.dropped_raw_queue_evicted,
            UdpDropReason::ReceiveQueueFull => &self.counters.dropped_receive_queue_full,
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

impl TransportHandle for UdpConnectionHandle {
    fn record_drop(&self, reason: QueueDropReason) {
        self.count_drop(match reason {
            QueueDropReason::OutgoingQueueFull => UdpDropReason::OutgoingQueueFull,
            QueueDropReason::OutgoingQueueEvicted => UdpDropReason::OutgoingQueueEvicted,
            QueueDropReason::ReceiveQueueFull => UdpDropReason::ReceiveQueueFull,
            QueueDropReason::ReceiveQueueEvicted => UdpDropReason::ReceiveQueueEvicted,
            QueueDropReason::ClosedBeforeDelivery => UdpDropReason::ClosedBeforeDelivery,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloned_handles_record_each_generic_queue_drop_in_shared_counters() {
        let handle = UdpConnectionHandle::new(1024, None);
        let clone = handle.clone();
        for reason in [
            QueueDropReason::OutgoingQueueFull,
            QueueDropReason::OutgoingQueueEvicted,
            QueueDropReason::ReceiveQueueFull,
            QueueDropReason::ReceiveQueueEvicted,
            QueueDropReason::ClosedBeforeDelivery,
        ] {
            clone.record_drop(reason);
        }
        assert_eq!(
            handle.stats(),
            UdpStatsSnapshot {
                dropped_outgoing_queue_full: 1,
                dropped_outgoing_queue_evicted: 1,
                dropped_receive_queue_full: 1,
                dropped_receive_queue_evicted: 1,
                dropped_closed_before_delivery: 1,
                ..UdpStatsSnapshot::default()
            }
        );
        drop(handle);
        clone.record_drop(QueueDropReason::ClosedBeforeDelivery);
        assert_eq!(clone.stats().dropped_closed_before_delivery, 2);
    }
}
