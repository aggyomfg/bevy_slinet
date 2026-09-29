use super::*;
use crate::connection::{ConnectionId, EcsConnection};
#[cfg(all(feature = "protocol_udp", any(feature = "client", feature = "server")))]
use crate::protocols::udp::UdpConnectionHandle;
use std::sync::{atomic::AtomicBool, Arc};
use tokio_util::sync::CancellationToken;

#[test]
fn outgoing_queue_reports_full_and_closed() {
    let (packet_tx, _receiver) = tokio::sync::mpsc::channel(1);
    let connection = EcsConnection {
        disconnect_task: CancellationToken::new(),
        id: ConnectionId::next(),
        published: Arc::new(AtomicBool::new(false)),
        packet_tx: OutgoingSender::reliable(packet_tx),
        transport: (),
        local_addr: "127.0.0.1:1".parse().unwrap(),
        peer_addr: "127.0.0.1:2".parse().unwrap(),
    };
    connection.send(1).unwrap();
    assert!(matches!(connection.send(2), Err(SendError::Full(2))));
    connection.disconnect();
    assert!(matches!(connection.send(3), Err(SendError::Closed(3))));
}

#[cfg(any(feature = "client", feature = "server"))]
#[tokio::test]
async fn udp_overflow_drops_new_packets_and_tcp_waits_cancel_safely() {
    let (sender, mut receiver) = lossy_channel(1, usize::MAX, OverflowPolicy::DropNewest);
    let cancel = CancellationToken::new();
    let udp = PacketForwarder::new(sender.clone(), true, cancel.clone(), ());
    assert!(udp.forward(1, |_| {}).await);
    assert!(udp.forward(2, |_| {}).await);
    assert_eq!(receiver.recv().await, Some(1));
    sender.send(3, 1).await.unwrap();
    let tcp = PacketForwarder::new(sender, false, cancel.clone(), ());
    let mut pending = Box::pin(tcp.forward(4, |_| {}));
    assert!(futures::poll!(pending.as_mut()).is_pending());
    cancel.cancel();
    assert!(!pending.await);
    assert_eq!(receiver.recv().await, Some(3));
}

#[cfg(all(feature = "protocol_udp", any(feature = "client", feature = "server")))]
#[tokio::test]
async fn udp_outgoing_policies_report_rejections_and_evictions() {
    let queues = NetworkQueueSettings {
        send_capacity: 1,
        datagram_send_overflow: OverflowPolicy::DropNewest,
        ..Default::default()
    };
    let (packet_tx, mut receiver) = queues.outgoing_channel::<_, UdpConnectionHandle>(true);
    let udp = UdpConnectionHandle::new(100, None);
    let connection = EcsConnection {
        disconnect_task: CancellationToken::new(),
        id: ConnectionId::next(),
        published: Arc::new(AtomicBool::new(false)),
        packet_tx,
        transport: udp.clone(),
        local_addr: "127.0.0.1:1".parse().unwrap(),
        peer_addr: "127.0.0.1:2".parse().unwrap(),
    };
    connection.send(1).unwrap();
    assert!(matches!(connection.send(2), Err(SendError::Full(2))));
    assert_eq!(receiver.recv().await, Some(1));
    assert_eq!(udp.stats().dropped_outgoing_queue_full, 1);

    let queues = NetworkQueueSettings {
        datagram_send_overflow: OverflowPolicy::DropOldest,
        ..queues
    };
    let (packet_tx, mut receiver) = queues.outgoing_channel::<_, UdpConnectionHandle>(true);
    let connection = EcsConnection {
        packet_tx,
        ..connection
    };
    connection.send(3).unwrap();
    connection.send(4).unwrap();
    assert_eq!(receiver.recv().await, Some(4));
    assert_eq!(udp.stats().dropped_outgoing_queue_evicted, 1);
    connection.disconnect();
    assert_eq!(
        connection
            .transport()
            .stats()
            .dropped_outgoing_queue_evicted,
        1
    );
}

#[cfg(all(feature = "protocol_udp", any(feature = "client", feature = "server")))]
#[tokio::test]
async fn udp_incoming_eviction_is_charged_to_discarded_session() {
    struct Packet {
        value: u8,
        transport: UdpConnectionHandle,
    }
    let older = UdpConnectionHandle::new(100, None);
    let newer = UdpConnectionHandle::new(100, None);
    let (tx, mut rx) = lossy_channel(1, usize::MAX, OverflowPolicy::DropOldest);
    let forwarder = PacketForwarder::new(tx, true, CancellationToken::new(), newer.clone());
    assert!(
        forwarder
            .forward(
                Packet {
                    value: 1,
                    transport: older.clone()
                },
                |discarded| {
                    discarded
                        .transport
                        .record_drop(QueueDropReason::ReceiveQueueEvicted);
                }
            )
            .await
    );
    assert!(
        forwarder
            .forward(
                Packet {
                    value: 2,
                    transport: newer.clone()
                },
                |discarded| {
                    discarded
                        .transport
                        .record_drop(QueueDropReason::ReceiveQueueEvicted);
                }
            )
            .await
    );
    assert_eq!(rx.recv().await.map(|packet| packet.value), Some(2));
    assert_eq!(older.stats().dropped_receive_queue_evicted, 1);
    assert_eq!(newer.stats().dropped_receive_queue_evicted, 0);
}

// A custom transport must receive queue reports without requiring a built-in
// protocol feature, Debug, or Default on its handle.
#[cfg(any(feature = "client", feature = "server"))]
#[derive(Clone)]
struct CustomHandle(Arc<std::sync::Mutex<Vec<QueueDropReason>>>);

#[cfg(any(feature = "client", feature = "server"))]
impl crate::protocols::protocol::TransportHandle for CustomHandle {
    fn record_drop(&self, reason: QueueDropReason) {
        self.0.lock().unwrap().push(reason);
    }
}

#[cfg(any(feature = "client", feature = "server"))]
#[test]
fn custom_handle_tracks_eviction_and_queued_packets_on_close() {
    let drops = Arc::new(std::sync::Mutex::new(Vec::new()));
    let handle = CustomHandle(Arc::clone(&drops));
    let settings = NetworkQueueSettings {
        send_capacity: 1,
        datagram_send_overflow: OverflowPolicy::DropOldest,
        ..Default::default()
    };
    let (packet_tx, mut receiver) = settings.outgoing_channel::<_, CustomHandle>(true);
    receiver.set_transport(handle.clone());
    let connection = EcsConnection {
        disconnect_task: CancellationToken::new(),
        id: ConnectionId::next(),
        published: Arc::new(AtomicBool::new(false)),
        packet_tx,
        transport: handle,
        local_addr: "127.0.0.1:1".parse().unwrap(),
        peer_addr: "127.0.0.1:2".parse().unwrap(),
    };
    let retained = connection.clone();
    connection.send(1).unwrap();
    retained.send(2).unwrap();
    connection.disconnect();
    assert!(matches!(
        retained.send(3),
        Err(crate::connection::SendError::Closed(3))
    ));
    drop(receiver);
    assert_eq!(
        *drops.lock().unwrap(),
        [
            QueueDropReason::OutgoingQueueEvicted,
            QueueDropReason::ClosedBeforeDelivery
        ],
    );
}

#[cfg(any(feature = "client", feature = "server"))]
#[test]
fn reliable_handle_reports_full_and_buffered_cancellation() {
    let drops = Arc::new(std::sync::Mutex::new(Vec::new()));
    let handle = CustomHandle(Arc::clone(&drops));
    let settings = NetworkQueueSettings {
        send_capacity: 1,
        ..Default::default()
    };
    let (packet_tx, mut receiver) = settings.outgoing_channel::<_, CustomHandle>(false);
    receiver.set_transport(handle.clone());
    let connection = EcsConnection {
        disconnect_task: CancellationToken::new(),
        id: ConnectionId::next(),
        published: Arc::new(AtomicBool::new(false)),
        packet_tx,
        transport: handle,
        local_addr: "127.0.0.1:1".parse().unwrap(),
        peer_addr: "127.0.0.1:2".parse().unwrap(),
    };
    connection.send(String::from("buffered")).unwrap();
    let error = connection.send(String::from("full")).unwrap_err();
    assert!(matches!(error, SendError::Full(_)));
    assert_eq!(error.into_inner(), "full");
    connection.disconnect();
    let error = connection.send(String::from("closed")).unwrap_err();
    assert!(matches!(error, SendError::Closed(_)));
    assert_eq!(error.into_inner(), "closed");
    drop(receiver);
    assert_eq!(
        *drops.lock().unwrap(),
        [
            QueueDropReason::OutgoingQueueFull,
            QueueDropReason::ClosedBeforeDelivery
        ],
    );
}

#[cfg(any(feature = "client", feature = "server"))]
#[test]
fn pending_packet_counts_only_cancellation_and_aborted_sends() {
    use crate::connection::transport::PendingPacket;
    let drops = Arc::new(std::sync::Mutex::new(Vec::new()));
    let handle = CustomHandle(Arc::clone(&drops));
    for (result, expected) in [
        (Some(Ok(())), 0),
        (Some(Err(std::io::Error::other("serialization failed"))), 0),
        (Some(Err(std::io::ErrorKind::ConnectionAborted.into())), 1),
        (None, 2),
    ] {
        let mut pending = PendingPacket::new(handle.clone());
        if let Some(result) = result {
            pending.finish(&result);
        }
        drop(pending);
        assert_eq!(drops.lock().unwrap().len(), expected);
    }
    assert!(drops
        .lock()
        .unwrap()
        .iter()
        .all(|reason| *reason == QueueDropReason::ClosedBeforeDelivery));
}
