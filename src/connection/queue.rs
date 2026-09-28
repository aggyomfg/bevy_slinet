//! Queue policy and bounded channels between ECS and transport tasks.

use bevy::prelude::Resource;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::mpsc::{Receiver, Sender};

#[cfg(any(feature = "client", feature = "server"))]
use crate::packet_queue::{lossy_channel, LossyReceiver, LossySender};

use super::transport::ConnectionDiagnostics;
#[cfg(any(feature = "client", feature = "server"))]
use super::{transport::QueueDropReason, DisconnectTask};

/// Chooses which queued datagram to discard when a UDP queue fills.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum OverflowPolicy {
    /// Reject the newly submitted item.
    #[default]
    DropNewest,
    /// Evict the oldest queued items to admit the new item.
    DropOldest,
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

    pub(super) fn try_send(
        &self,
        packet: T,
        diagnostics: &ConnectionDiagnostics,
    ) -> Result<(), TrySendError<T>> {
        #[cfg(not(any(feature = "client", feature = "server")))]
        let _ = diagnostics;
        match &self.0 {
            OutgoingSenderInner::Reliable(tx) => tx.try_send(packet),
            #[cfg(any(feature = "client", feature = "server"))]
            OutgoingSenderInner::Lossy(tx) => match tx.try_send(packet, 1) {
                Ok(evicted) => {
                    for _ in evicted {
                        diagnostics.record_drop(QueueDropReason::OutgoingQueueEvicted);
                    }
                    Ok(())
                }
                Err(err) => {
                    if matches!(err, TrySendError::Full(_)) {
                        diagnostics.record_drop(QueueDropReason::OutgoingQueueFull);
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
    #[cfg(any(feature = "client", feature = "server"))]
    diagnostics: ConnectionDiagnostics,
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
            #[cfg(any(feature = "client", feature = "server"))]
            diagnostics: ConnectionDiagnostics::default(),
        }
    }
}

impl<T> OutgoingReceiver<T> {
    #[cfg(any(feature = "client", feature = "server"))]
    #[cfg_attr(
        not(feature = "protocol_udp"),
        expect(
            clippy::missing_const_for_fn,
            reason = "Replacing enabled UDP diagnostics drops a shared handle"
        )
    )]
    pub(crate) fn set_diagnostics(&mut self, diagnostics: ConnectionDiagnostics) {
        self.diagnostics = diagnostics;
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
        #[cfg(any(feature = "client", feature = "server"))]
        if let OutgoingReceiverInner::Lossy(rx) = &mut self.inner {
            let pending = rx.close_and_drain();
            for _ in &pending {
                self.diagnostics
                    .record_drop(QueueDropReason::ClosedBeforeDelivery);
            }
            drop(pending);
        }
    }
}

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
                    diagnostics: ConnectionDiagnostics::default(),
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
pub struct PacketForwarder<T> {
    sender: LossySender<T>,
    datagram: bool,
    cancel: DisconnectTask,
    diagnostics: ConnectionDiagnostics,
}
#[cfg(any(feature = "client", feature = "server"))]
impl<T> PacketForwarder<T> {
    pub(crate) const fn new(
        sender: LossySender<T>,
        datagram: bool,
        cancel: DisconnectTask,
        diagnostics: ConnectionDiagnostics,
    ) -> Self {
        Self {
            sender,
            datagram,
            cancel,
            diagnostics,
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
                    self.diagnostics
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
