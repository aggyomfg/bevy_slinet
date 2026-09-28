//! Errors from submitting packets to a connection's outgoing queue.

use std::error::Error;
use std::fmt::{self, Debug, Display, Formatter};

/// A rejected outgoing packet. Successful queueing does not imply peer delivery.
#[derive(Debug, Eq, PartialEq)]
pub enum SendError<T> {
    /// The queue has no capacity for this packet.
    Full(T),
    /// The connection or its outgoing queue has closed.
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

impl<T> Display for SendError<T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Full(_) => f.write_str("outgoing packet queue is full"),
            Self::Closed(_) => f.write_str("connection is closed"),
        }
    }
}

impl<T: Debug> Error for SendError<T> {}
