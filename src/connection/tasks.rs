//! Owned state passed to the built-in client and server packet tasks.

use bevy::log;
use futures::StreamExt;
use std::future::Future;
use std::{fmt::Debug, sync::Arc};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::{transport::PendingPacket, ConnectionId, OutgoingReceiver, ReceiveLimits};
use crate::protocols::protocol::{PacketWriter, TransportHandle};
use crate::serializers::serializer::Serializer;
use crate::PacketLengthSerializer;

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

// A custom into_split() may perform an asynchronous handshake. Poll independent
// connections concurrently, with backpressure once the setup budget is full.
pub async fn run_connections<T, F, Fut>(receiver: mpsc::Receiver<T>, limit: usize, run: F)
where
    F: FnMut(T) -> Fut,
    Fut: Future<Output = ()>,
{
    futures::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|value| (value, receiver))
    })
    .for_each_concurrent(limit.max(1), run)
    .await;
}

pub async fn send_packets<R, P, S, LS, H>(
    mut write: impl PacketWriter,
    codecs: PacketCodecs<S, LS>,
    state: SendTaskState<P, H>,
) where
    R: Send + Sync + Debug + 'static,
    P: Send + Sync + Debug + 'static,
    S: Serializer<R, P> + ?Sized,
    LS: PacketLengthSerializer,
    H: TransportHandle,
{
    let PacketCodecs {
        serializer,
        packet_length_serializer,
    } = codecs;
    let SendTaskState {
        mut packets_rx,
        disconnect_task,
        id,
        transport,
        send_error,
    } = state;
    let _guard = disconnect_task.clone().drop_guard();
    let sending = async {
        while let Some(packet) = packets_rx.recv().await {
            let mut pending = PendingPacket::new(transport.clone());
            if disconnect_task.is_cancelled() {
                break;
            }
            log::trace!("({id:?}) Sending packet {packet:?}");
            let result = write
                .send(packet, Arc::clone(&serializer), &*packet_length_serializer)
                .await;
            pending.finish(&result);
            if let Err(err) = result {
                log::error!("({id:?}) Error sending packet: {err}");
                let _ = send_error.send(err);
                break;
            }
        }
    };
    tokio::select! {
        biased;
        () = disconnect_task.cancelled() => {},
        () = sending => {},
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connection_setup_progresses_independently_with_bounded_concurrency() {
        let (connections, receiver) = mpsc::channel(3);
        let (started, mut starts) = mpsc::unbounded_channel();
        let mut releases = Vec::new();
        for id in 0..3 {
            let (release, wait) = oneshot::channel::<()>();
            releases.push(release);
            connections.send((id, wait)).await.unwrap();
        }
        drop(connections);
        let task = run_connections(receiver, 2, |(id, wait)| {
            let started = started.clone();
            async move {
                started.send(id).unwrap();
                let _ = wait.await;
            }
        });
        futures::pin_mut!(task);
        assert!(futures::poll!(&mut task).is_pending());
        assert_eq!(starts.try_recv().unwrap(), 0);
        assert_eq!(starts.try_recv().unwrap(), 1);
        assert!(starts.try_recv().is_err());

        // The second setup can finish and make room while the first is stalled.
        releases.remove(1).send(()).unwrap();
        assert!(futures::poll!(&mut task).is_pending());
        assert_eq!(starts.try_recv().unwrap(), 2);
        drop(releases);
        assert!(futures::poll!(&mut task).is_ready());
    }
}
