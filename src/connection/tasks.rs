//! Owned state passed to the built-in client and server packet tasks.

use std::sync::Arc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::{ConnectionId, OutgoingReceiver, ReceiveLimits};
use crate::protocols::protocol::TransportHandle;

/// Payload and framing codecs shared by a connection's read and write tasks.
pub struct PacketCodecs<S: ?Sized, LS> {
    pub serializer: Arc<S>,
    pub packet_length_serializer: Arc<LS>,
}

/// Read-side decoding, limits and writer failure reporting.
pub struct ReceiveTaskState<S: ?Sized, LS> {
    pub codecs: PacketCodecs<S, LS>,
    pub receive_limits: ReceiveLimits,
    pub send_error: oneshot::Receiver<std::io::Error>,
}

/// Outgoing queue and the connection state needed to drive it.
/// Keeps only the writer's handles, without retaining an outgoing queue sender.
pub struct SendTaskState<P, H: TransportHandle> {
    pub packets_rx: OutgoingReceiver<P, H>,
    pub disconnect_task: CancellationToken,
    pub id: ConnectionId,
    pub transport: H,
    pub send_error: oneshot::Sender<std::io::Error>,
}
