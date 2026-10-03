//! Shared send cancellation accounting.

#[cfg(any(feature = "client", feature = "server"))]
use crate::protocols::protocol::{QueueDropReason, TransportHandle};

/// Counts a dequeued packet if its send is interrupted before completion.
#[cfg(any(feature = "client", feature = "server"))]
pub struct PendingPacket<H: TransportHandle>(Option<H>);

#[cfg(any(feature = "client", feature = "server"))]
impl<H: TransportHandle> PendingPacket<H> {
    pub const fn new(transport: H) -> Self {
        Self(Some(transport))
    }

    pub fn finish(&mut self, result: &std::io::Result<()>) {
        // Other failures are handled by the transport or serialization layer.
        // Completion does not imply a successful socket send or remote delivery.
        if !matches!(result, Err(err) if err.kind() == std::io::ErrorKind::ConnectionAborted) {
            self.0 = None;
        }
    }
}

#[cfg(any(feature = "client", feature = "server"))]
impl<H: TransportHandle> Drop for PendingPacket<H> {
    fn drop(&mut self) {
        if let Some(transport) = self.0.take() {
            transport.record_drop(QueueDropReason::ClosedBeforeDelivery);
        }
    }
}
