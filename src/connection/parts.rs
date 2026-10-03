//! Owned parts for custom connection task implementations.
use super::{ConnectionId, OutgoingReceiver, ReceiveLimits};
use crate::protocols::protocol::NetworkStream;
use crate::serializers::{
    packet_length_serializer::PacketLengthSerializer, serializer::Serializer,
};
use std::{error::Error, fmt::Debug, sync::Arc};
use tokio_util::sync::CancellationToken;

/// All state extracted by [`super::RawConnection::into_parts`].
///
/// The caller takes responsibility for cancellation, packet I/O and receive limits.
/// Dropping `packets_rx` closes the outgoing queue and accounts for undelivered packets.
pub struct RawConnectionParts<ReceivingPacket, SendingPacket, NS, EncErr, DecErr, LS>
where
    ReceivingPacket: Send + Sync + Debug + 'static,
    SendingPacket: Send + Sync + Debug + 'static,
    NS: NetworkStream,
    EncErr: Error + Send + Sync,
    DecErr: Error + Send + Sync,
    LS: PacketLengthSerializer,
{
    /// Shared cancellation signal. A custom task must observe this token.
    pub disconnect_task: CancellationToken,
    /// Owned transport; split it to run packet I/O.
    pub stream: NS,
    /// Shared payload encoder and decoder.
    pub serializer: Arc<
        dyn Serializer<ReceivingPacket, SendingPacket, EncodeError = EncErr, DecodeError = DecErr>,
    >,
    /// Shared stream framing codec.
    pub packet_length_serializer: Arc<LS>,
    /// Outgoing packet queue, retaining its transport drop accounting.
    pub packets_rx: OutgoingReceiver<SendingPacket, NS::Handle>,
    /// Live incoming payload limit shared with the endpoint.
    pub receive_limits: ReceiveLimits,
    /// Original local connection identity.
    pub id: ConnectionId,
}
