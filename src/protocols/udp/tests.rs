use super::*;
use crate::{
    connection::{OverflowPolicy, ReceiveLimits},
    protocol::{ClientStream, Listener, NetworkStream, PacketReader, PacketWriter},
    serializers::{packet_length_serializer::LittleEndian, serializer::Serializer},
};
use bevy::platform::time::Instant;
use std::{io, sync::Arc};
use tokio::net::UdpSocket;
type Length = LittleEndian<u32>;
struct Raw;
impl Serializer<Vec<u8>, Vec<u8>> for Raw {
    type EncodeError = io::Error;
    type DecodeError = io::Error;
    fn serialize(&self, packet: Vec<u8>) -> io::Result<Vec<u8>> {
        Ok(packet)
    }
    fn deserialize(&self, bytes: &[u8]) -> io::Result<Vec<u8>> {
        Ok(bytes.to_vec())
    }
}

#[tokio::test]
async fn peer_capacity_includes_unsplit_streams_and_is_reusable() {
    let listener = UdpNetworkListener::bind(
        "127.0.0.1:0".parse().unwrap(),
        UdpOptions {
            max_peers: 1,
            ..UdpOptions::DEFAULT
        },
    )
    .await
    .unwrap();
    let a = "127.0.0.1:10001".parse().unwrap();
    let b = "127.0.0.1:10002".parse().unwrap();
    let first = listener.dispatch(&[], a, Instant::now()).unwrap();
    assert!(listener.dispatch(&[], b, Instant::now()).is_none());
    drop(first);
    assert!(listener.dispatch(&[], b, Instant::now()).is_some());
}

#[tokio::test]
async fn old_registration_cannot_remove_replacement_and_old_writer_is_cancelled() {
    let listener = UdpNetworkListener::bind("127.0.0.1:0".parse().unwrap(), UdpOptions::DEFAULT)
        .await
        .unwrap();
    let a = "127.0.0.1:10001".parse().unwrap();
    let (mut old_read, mut old_write) = listener
        .dispatch(&[1], a, Instant::now())
        .unwrap()
        .into_split()
        .await
        .unwrap();
    old_read.close();
    let (mut read, _write) = listener
        .dispatch(&[2], a, Instant::now())
        .unwrap()
        .into_split()
        .await
        .unwrap();
    drop(old_read);
    assert!(listener.dispatch(&[3], a, Instant::now()).is_none());
    for expected in [vec![2], vec![3]] {
        assert_eq!(
            read.receive(Arc::new(Raw), &Length::default(), &ReceiveLimits::default())
                .await
                .unwrap(),
            expected
        );
    }
    assert_eq!(
        old_write
            .send(vec![4], Arc::new(Raw), &Length::default())
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::ConnectionAborted
    );
}

#[tokio::test]
async fn listener_drop_wakes_reader_with_retained_writer() {
    let listener = UdpNetworkListener::bind("127.0.0.1:0".parse().unwrap(), UdpOptions::DEFAULT)
        .await
        .unwrap();
    let (mut read, _write) = listener
        .dispatch(&[], "127.0.0.1:10001".parse().unwrap(), Instant::now())
        .unwrap()
        .into_split()
        .await
        .unwrap();
    assert!(read
        .receive(Arc::new(Raw), &Length::default(), &ReceiveLimits::default())
        .await
        .unwrap()
        .is_empty());
    let length = Length::default();
    let limits = ReceiveLimits::default();
    let mut pending = Box::pin(read.receive(Arc::new(Raw), &length, &limits));
    assert!(futures::poll!(pending.as_mut()).is_pending());
    drop(listener);
    let result = tokio::time::timeout(std::time::Duration::from_secs(1), pending)
        .await
        .unwrap();
    assert!(result.is_err());
}

#[tokio::test]
async fn rejected_first_datagram_does_not_consume_peer_capacity() {
    for policy in [OverflowPolicy::DropNewest, OverflowPolicy::DropOldest] {
        let listener = UdpNetworkListener::bind(
            "127.0.0.1:0".parse().unwrap(),
            UdpOptions {
                max_peers: 1,
                receive_queue_bytes: 1,
                receive_queue_overflow: policy,
                ..UdpOptions::DEFAULT
            },
        )
        .await
        .unwrap();
        let a = "127.0.0.1:10001".parse().unwrap();
        assert!(listener.dispatch(&[1, 2], a, Instant::now()).is_none());
        assert!(listener.dispatch(&[], a, Instant::now()).is_some());
    }
}

#[tokio::test]
async fn zero_outgoing_budget_allows_empty_and_drops_nonempty() {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let stream = UdpClientStream::connect_with_options(
        socket.local_addr().unwrap(),
        UdpOptions {
            max_datagram_size: 0,
            ..UdpOptions::DEFAULT
        },
    )
    .await
    .unwrap();
    let handle = stream.transport();
    let (_read, mut write) = stream.into_split().await.unwrap();
    write
        .send(vec![1], Arc::new(Raw), &Length::default())
        .await
        .unwrap();
    write
        .send(vec![], Arc::new(Raw), &Length::default())
        .await
        .unwrap();
    assert_eq!(socket.recv(&mut [0; 4]).await.unwrap(), 0);
    assert_eq!(handle.stats().dropped_oversized_payload, 1);
    assert_eq!(handle.stats().sent_data_packets, 1);
    assert_eq!(handle.stats().sent_data_bytes, 0);
}

#[tokio::test]
async fn ipv6_peer_preserves_datagrams_and_exposes_full_payload_budget() {
    super::test_utils::wall_timeout(async {
        let listener =
            match UdpNetworkListener::bind("[::1]:0".parse().unwrap(), UdpOptions::DEFAULT).await {
                Ok(listener) => listener,
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::AddrNotAvailable | io::ErrorKind::Unsupported
                    ) =>
                {
                    return
                }
                Err(err) => panic!("IPv6 bind failed: {err}"),
            };
        let client = UdpClientStream::connect(listener.address()).await.unwrap();
        assert_eq!(client.transport().max_payload_size(), 1200);
        let (mut cr, mut cw) = client.into_split().await.unwrap();
        cw.send(vec![1, 2], Arc::new(Raw), &Length::default())
            .await
            .unwrap();
        let (mut sr, mut sw) = listener.accept().await.unwrap().into_split().await.unwrap();
        assert_eq!(
            sr.receive(Arc::new(Raw), &Length::default(), &ReceiveLimits::default())
                .await
                .unwrap(),
            vec![1, 2]
        );
        sw.send(vec![3], Arc::new(Raw), &Length::default())
            .await
            .unwrap();
        assert_eq!(
            cr.receive(Arc::new(Raw), &Length::default(), &ReceiveLimits::default())
                .await
                .unwrap(),
            vec![3]
        );
    })
    .await;
}
