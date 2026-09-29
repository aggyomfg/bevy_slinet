//! Queue policy and bounded channels between ECS and transport tasks.

use bevy::prelude::Resource;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{Receiver, Sender};

#[cfg(any(feature = "client", feature = "server"))]
use crate::packet_queue::{lossy_channel, LossyReceiver, LossySender};

use super::SendError;
use crate::protocols::protocol::{QueueDropReason, TransportHandle};
#[cfg(any(feature = "client", feature = "server"))]
use tokio_util::sync::CancellationToken;

/// Chooses which queued datagram to discard when a datagram queue fills.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OverflowPolicy {
    /// Reject the newly submitted item.
    #[default]
    DropNewest,
    /// Evict the oldest queued items to admit the new item.
    DropOldest,
}

/// A momentary view of an outgoing packet queue.
///
/// This is diagnostic information, not a reservation or delivery acknowledgement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueSnapshot {
    /// Occupied packet slots. Concurrent sends can transiently occupy a slot
    /// before publishing its packet. Excludes packets already taken by the writer.
    pub queued: usize,
    /// Maximum packet slots, including occupied slots.
    pub capacity: usize,
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
    pub(super) fn is_closed(&self) -> bool {
        match &self.0 {
            OutgoingSenderInner::Reliable(tx) => tx.is_closed(),
            #[cfg(any(feature = "client", feature = "server"))]
            OutgoingSenderInner::Lossy(tx) => tx.is_closed(),
        }
    }

    pub(super) fn snapshot(&self) -> QueueSnapshot {
        match &self.0 {
            OutgoingSenderInner::Reliable(tx) => {
                let capacity = tx.max_capacity();
                QueueSnapshot {
                    queued: capacity - tx.capacity(),
                    capacity,
                }
            }
            #[cfg(any(feature = "client", feature = "server"))]
            OutgoingSenderInner::Lossy(tx) => tx.snapshot(),
        }
    }

    #[cfg(any(feature = "client", feature = "server", test))]
    const fn reliable(tx: Sender<T>) -> Self {
        Self(OutgoingSenderInner::Reliable(tx))
    }

    #[cfg(any(feature = "client", feature = "server"))]
    const fn lossy(tx: LossySender<T>) -> Self {
        Self(OutgoingSenderInner::Lossy(tx))
    }

    pub(super) fn try_send(
        &self,
        packet: T,
        transport: &impl TransportHandle,
    ) -> Result<(), SendError<T>> {
        let result = match &self.0 {
            OutgoingSenderInner::Reliable(tx) => tx.try_send(packet),
            #[cfg(any(feature = "client", feature = "server"))]
            OutgoingSenderInner::Lossy(tx) => match tx.try_send(packet, 1) {
                Ok(evicted) => {
                    for _ in evicted {
                        transport.record_drop(QueueDropReason::OutgoingQueueEvicted);
                    }
                    Ok(())
                }
                Err(err) => Err(err),
            },
        };
        result.map_err(|err| match err {
            TrySendError::Full(packet) => {
                transport.record_drop(QueueDropReason::OutgoingQueueFull);
                SendError::Full(packet)
            }
            TrySendError::Closed(packet) => SendError::Closed(packet),
        })
    }
}

/// Receiving endpoint for a transport-specific outgoing packet queue.
pub struct OutgoingReceiver<T, H: TransportHandle> {
    inner: OutgoingReceiverInner<T>,
    transport: Option<H>,
}

#[cfg_attr(not(any(feature = "client", feature = "server")), allow(dead_code))]
enum OutgoingReceiverInner<T> {
    Reliable(Receiver<T>),
    #[cfg(any(feature = "client", feature = "server"))]
    Lossy(LossyReceiver<T>),
}

impl<T, H: TransportHandle> From<Receiver<T>> for OutgoingReceiver<T, H> {
    fn from(receiver: Receiver<T>) -> Self {
        Self {
            inner: OutgoingReceiverInner::Reliable(receiver),
            transport: None,
        }
    }
}

impl<T, H: TransportHandle> OutgoingReceiver<T, H> {
    pub(crate) fn set_transport(&mut self, transport: H) {
        self.transport = Some(transport);
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

impl<T, H: TransportHandle> Drop for OutgoingReceiver<T, H> {
    fn drop(&mut self) {
        let Some(transport) = &self.transport else {
            return;
        };
        match &mut self.inner {
            OutgoingReceiverInner::Reliable(rx) => {
                rx.close();
                while rx.try_recv().is_ok() {
                    transport.record_drop(QueueDropReason::ClosedBeforeDelivery);
                }
            }
            #[cfg(any(feature = "client", feature = "server"))]
            OutgoingReceiverInner::Lossy(rx) => {
                let pending = rx.close_and_drain();
                for _ in &pending {
                    transport.record_drop(QueueDropReason::ClosedBeforeDelivery);
                }
                drop(pending);
            }
        }
    }
}

/// Limits the channels between ECS and network tasks. Insert before `Startup`.
///
/// Capacities count packets/events (not decoded bytes); zero capacities are clamped to one.
/// Datagram transports apply the selected overflow policies; stream packets apply backpressure.
#[derive(Clone, Copy, Debug, Resource)]
pub struct NetworkQueueSettings {
    /// Outgoing packets per connection. Datagram `DropNewest` reports `Full` on overflow.
    pub send_capacity: usize,
    /// Incoming channel capacity per plugin. Streams share a FIFO for packets and
    /// lifecycle; datagrams have a separate packet queue.
    pub receive_capacity: usize,
    /// Maximum events drained per frame. Streams share this budget across packets,
    /// establishment and closure. Datagram lifecycle and packets have separate budgets.
    /// May change at runtime; zero pauses delivery.
    pub events_per_frame: usize,
    /// Policy for a full datagram outgoing packet queue.
    pub datagram_send_overflow: OverflowPolicy,
    /// Policy for a full datagram receive event queue.
    pub datagram_receive_overflow: OverflowPolicy,
}

impl Default for NetworkQueueSettings {
    fn default() -> Self {
        Self {
            send_capacity: 1024,
            receive_capacity: 4096,
            events_per_frame: 256,
            datagram_send_overflow: OverflowPolicy::DropNewest,
            datagram_receive_overflow: OverflowPolicy::DropNewest,
        }
    }
}

#[cfg(any(feature = "client", feature = "server"))]
impl NetworkQueueSettings {
    pub(crate) fn outgoing_channel<T, H: TransportHandle>(
        &self,
        datagram: bool,
    ) -> (OutgoingSender<T>, OutgoingReceiver<T, H>) {
        if datagram {
            let (tx, rx) = lossy_channel(
                self.send_capacity.max(1),
                usize::MAX,
                self.datagram_send_overflow,
            );
            (
                OutgoingSender::lossy(tx),
                OutgoingReceiver {
                    inner: OutgoingReceiverInner::Lossy(rx),
                    transport: None,
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
pub struct PacketForwarder<T, H: TransportHandle> {
    sender: LossySender<T>,
    datagram: bool,
    cancel: CancellationToken,
    transport: H,
}
#[cfg(any(feature = "client", feature = "server"))]
impl<T, H: TransportHandle> PacketForwarder<T, H> {
    pub(crate) const fn new(
        sender: LossySender<T>,
        datagram: bool,
        cancel: CancellationToken,
        transport: H,
    ) -> Self {
        Self {
            sender,
            datagram,
            cancel,
            transport,
        }
    }

    /// Returns false when forwarding must stop; datagram queue overflow drops the packet.
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
                    self.transport
                        .record_drop(QueueDropReason::ReceiveQueueFull);
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
#[path = "queue_tests.rs"]
mod tests;
