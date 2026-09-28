use super::*;
use crate::connection::{ConnectionId, DisconnectTask, EcsConnection};
#[cfg(all(feature = "protocol_udp", any(feature = "client", feature = "server")))]
use crate::protocols::udp::UdpConnectionHandle;
use std::sync::{atomic::AtomicBool, Arc};

#[test]
fn outgoing_queue_reports_full_and_closed() {
    let (packet_tx, _receiver) = tokio::sync::mpsc::channel(1);
    let connection = EcsConnection {
        disconnect_task: DisconnectTask::new(),
        id: ConnectionId::next(),
        published: Arc::new(AtomicBool::new(false)),
        packet_tx: OutgoingSender::reliable(packet_tx),
        diagnostics: ConnectionDiagnostics::default(),
        local_addr: "127.0.0.1:1".parse().unwrap(),
        peer_addr: "127.0.0.1:2".parse().unwrap(),
    };
    connection.send(1).unwrap();
    assert!(matches!(connection.send(2), Err(TrySendError::Full(2))));
    connection.disconnect();
    assert!(matches!(connection.send(3), Err(TrySendError::Closed(3))));
}

#[cfg(any(feature = "client", feature = "server"))]
#[tokio::test]
async fn udp_overflow_drops_new_packets_and_tcp_waits_cancel_safely() {
    let (sender, mut receiver) = lossy_channel(1, usize::MAX, OverflowPolicy::DropNewest);
    let cancel = DisconnectTask::new();
    let udp = PacketForwarder::new(
        sender.clone(),
        true,
        cancel.clone(),
        ConnectionDiagnostics::default(),
    );
    assert!(udp.forward(1, |_| {}).await);
    assert!(udp.forward(2, |_| {}).await);
    assert_eq!(receiver.recv().await, Some(1));
    sender.send(3, 1).await.unwrap();
    let tcp = PacketForwarder::new(
        sender,
        false,
        cancel.clone(),
        ConnectionDiagnostics::default(),
    );
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
        udp_send_overflow: OverflowPolicy::DropNewest,
        ..Default::default()
    };
    let (packet_tx, mut receiver) = queues.outgoing_channel(true);
    let udp = UdpConnectionHandle::new(100, None);
    let connection = EcsConnection {
        disconnect_task: DisconnectTask::new(),
        id: ConnectionId::next(),
        published: Arc::new(AtomicBool::new(false)),
        packet_tx,
        diagnostics: ConnectionDiagnostics::from_udp(Some(udp.clone())),
        local_addr: "127.0.0.1:1".parse().unwrap(),
        peer_addr: "127.0.0.1:2".parse().unwrap(),
    };
    connection.send(1).unwrap();
    assert!(matches!(connection.send(2), Err(TrySendError::Full(2))));
    assert_eq!(receiver.recv().await, Some(1));
    assert_eq!(udp.stats().dropped_outgoing_queue_full, 1);

    let queues = NetworkQueueSettings {
        udp_send_overflow: OverflowPolicy::DropOldest,
        ..queues
    };
    let (packet_tx, mut receiver) = queues.outgoing_channel(true);
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
            .udp()
            .unwrap()
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
        diagnostics: ConnectionDiagnostics,
    }
    let older = UdpConnectionHandle::new(100, None);
    let newer = UdpConnectionHandle::new(100, None);
    let (tx, mut rx) = lossy_channel(1, usize::MAX, OverflowPolicy::DropOldest);
    let forwarder = PacketForwarder::new(
        tx,
        true,
        DisconnectTask::new(),
        ConnectionDiagnostics::from_udp(Some(newer.clone())),
    );
    assert!(
        forwarder
            .forward(
                Packet {
                    value: 1,
                    diagnostics: ConnectionDiagnostics::from_udp(Some(older.clone()))
                },
                |discarded| {
                    discarded
                        .diagnostics
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
                    diagnostics: ConnectionDiagnostics::from_udp(Some(newer.clone()))
                },
                |discarded| {
                    discarded
                        .diagnostics
                        .record_drop(QueueDropReason::ReceiveQueueEvicted);
                }
            )
            .await
    );
    assert_eq!(rx.recv().await.map(|packet| packet.value), Some(2));
    assert_eq!(older.stats().dropped_receive_queue_evicted, 1);
    assert_eq!(newer.stats().dropped_receive_queue_evicted, 0);
}
