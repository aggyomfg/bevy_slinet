/// A packet discarded by a bounded application queue or a closing connection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueDropReason {
    /// An outgoing packet was rejected because its queue was full.
    OutgoingQueueFull,
    /// An outgoing packet was evicted to make room for a newer packet.
    OutgoingQueueEvicted,
    /// A decoded packet was rejected because its receive queue was full.
    ReceiveQueueFull,
    /// A decoded packet was evicted to make room for a newer packet.
    ReceiveQueueEvicted,
    /// A packet was discarded because the connection closed before delivery.
    ClosedBeforeDelivery,
}

/// Shared controls and diagnostics for one transport connection.
///
/// Clones should refer to the same connection state and may outlive its network stream.
/// Transports without controls or diagnostics can use `()`.
pub trait TransportHandle: Clone + Send + Sync + 'static {
    /// Records a packet discarded by an application queue or a closing connection.
    /// The default implementation does nothing.
    fn record_drop(&self, _reason: QueueDropReason) {}
}

impl TransportHandle for () {}
