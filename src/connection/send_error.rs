//! Errors from submitting packets to a connection's outgoing queue.

/// A rejected outgoing packet. Successful queueing does not imply peer delivery.
#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub enum SendError<T> {
    /// The queue has no capacity for this packet.
    #[error("outgoing packet queue is full")]
    Full(T),
    /// The connection or its outgoing queue has closed.
    #[error("connection is closed")]
    Closed(T),
}

impl<T> SendError<T> {
    /// Returns ownership of the packet that could not be queued.
    #[must_use]
    pub fn into_inner(self) -> T {
        match self {
            Self::Full(packet) | Self::Closed(packet) => packet,
        }
    }
}
