use super::pacing::DataPacer;
use super::wire::{Frame, HandshakeFrame, Nonce};
use super::{UdpNetworkListener, UdpOptions};
use crate::{
    connection::ReceiveLimits,
    protocols::protocol::{NetworkStream, PacketReader},
    serializers::{packet_length_serializer::LittleEndian, serializer::Serializer},
};
use bevy::platform::time::Instant;
use std::{io, num::NonZeroU64, sync::Arc, time::Duration};
use tokio::{net::UdpSocket, sync::watch};
use tokio_util::sync::CancellationToken;

type Length = LittleEndian<u32>;

async fn recv_with_wall_deadline(
    socket: &UdpSocket,
    buffer: &mut [u8],
) -> (usize, std::net::SocketAddr) {
    let started = std::time::Instant::now();
    loop {
        match socket.try_recv_from(buffer) {
            Ok(received) => return received,
            Err(err)
                if err.kind() == io::ErrorKind::WouldBlock
                    && started.elapsed() < Duration::from_secs(3) =>
            {
                tokio::task::yield_now().await;
            }
            Err(err) => panic!("UDP test receive did not complete: {err}"),
        }
    }
}

struct Raw;
impl Serializer<Vec<u8>, Vec<u8>> for Raw {
    type EncodeError = io::Error;
    type DecodeError = io::Error;
    fn serialize(&self, packet: Vec<u8>) -> io::Result<Vec<u8>> {
        Ok(packet)
    }
    fn deserialize(&self, bytes: &[u8]) -> io::Result<Vec<u8>> {
        if bytes.first() == Some(&255) {
            Err(io::ErrorKind::InvalidData.into())
        } else {
            Ok(bytes.to_vec())
        }
    }
}

#[tokio::test]
#[expect(
    clippy::significant_drop_tightening,
    reason = "The stream is consumed by into_split and its halves must remain live"
)]
async fn receive_limit_and_malformed_payloads_are_counted_per_session() {
    super::test_support::wall_timeout(async {
        let listener =
            UdpNetworkListener::bind("127.0.0.1:0".parse().unwrap(), UdpOptions::DEFAULT)
                .await
                .unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = peer.local_addr().unwrap();
        let cookie = listener.issue_cookie(address, Nonce::from_bytes([13; 16]));
        let stream = listener
            .dispatch(
                &HandshakeFrame::confirm(cookie).encode(),
                address,
                Instant::now(),
            )
            .unwrap();
        let handle = stream.transport();
        assert_eq!(
            handle.max_payload_size(),
            UdpOptions::DEFAULT.max_payload_size().unwrap()
        );
        let (mut read, _write) = stream.into_split().await.unwrap();

        let malformed_frame = b"SLN2\x09"
            .iter()
            .copied()
            .chain([0; 32])
            .collect::<Vec<_>>();
        listener.dispatch(&malformed_frame, address, Instant::now());
        for payload in [&[1, 2][..], &[255][..], &[7][..]] {
            listener.dispatch(
                &Frame::data(cookie.mac, payload).encode(),
                address,
                Instant::now(),
            );
        }
        let limits = ReceiveLimits::new(1);
        assert_eq!(
            read.receive::<Vec<u8>, Vec<u8>, _, _>(Arc::new(Raw), &Length::default(), &limits)
                .await
                .unwrap(),
            vec![7]
        );
        let stats = handle.stats();
        assert_eq!(stats.received_data_packets, 3);
        assert_eq!(stats.received_data_bytes, 3 * 37 + 4);
        assert_eq!(stats.dropped_receive_limit, 1);
        assert_eq!(stats.dropped_malformed_payload, 2);
        drop(read);
        assert_eq!(handle.stats(), stats);
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn pacing_uses_wire_bytes_without_idle_credit() {
    let (rate, updates) = watch::channel(Some(NonZeroU64::new(100).unwrap()));
    let mut pacer = DataPacer::new(updates);
    let closed = CancellationToken::new();
    pacer.wait(&closed).await.unwrap();
    pacer.sent(100);
    let mut waiting = Box::pin(pacer.wait(&closed));
    assert!(futures::poll!(waiting.as_mut()).is_pending());
    tokio::time::advance(Duration::from_millis(999)).await;
    assert!(futures::poll!(waiting.as_mut()).is_pending());
    tokio::time::advance(Duration::from_millis(1)).await;
    waiting.await.unwrap();

    tokio::time::advance(Duration::from_secs(30)).await;
    pacer.wait(&closed).await.unwrap();
    pacer.sent(100);
    let mut next = Box::pin(pacer.wait(&closed));
    assert!(futures::poll!(next.as_mut()).is_pending());
    rate.send_replace(None);
    next.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn live_rate_update_and_close_wake_a_paced_send() {
    let (rate, updates) = watch::channel(Some(NonZeroU64::new(1).unwrap()));
    let mut pacer = DataPacer::new(updates);
    let closed = CancellationToken::new();
    pacer.sent(100);
    let mut waiting = Box::pin(pacer.wait(&closed));
    assert!(futures::poll!(waiting.as_mut()).is_pending());
    tokio::time::advance(Duration::from_millis(500)).await;
    rate.send_replace(Some(NonZeroU64::new(1_000).unwrap()));
    waiting.await.unwrap();

    pacer.sent(100);
    rate.send_replace(Some(NonZeroU64::new(1).unwrap()));
    let mut cancelled = Box::pin(pacer.wait(&closed));
    assert!(futures::poll!(cancelled.as_mut()).is_pending());
    closed.cancel();
    assert_eq!(
        cancelled.await.unwrap_err().kind(),
        io::ErrorKind::ConnectionAborted
    );
}

#[tokio::test(start_paused = true)]
async fn disconnect_attempts_are_spaced_one_second_apart() {
    super::test_support::wall_timeout(async {
        use super::session::{Heartbeat, SessionState};
        use super::settings::ValidatedOptions;
        use super::wire::{Control, Cookie, Nonce, Payload};

        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sender = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        sender
            .connect(receiver.local_addr().unwrap())
            .await
            .unwrap();
        let state = SessionState::new(
            Cookie::hello(Nonce::from_bytes([1; 16])),
            ValidatedOptions::new(UdpOptions::DEFAULT).unwrap(),
        );
        Heartbeat::new(sender, None, Arc::clone(&state)).spawn();
        state.close();
        let mut buffer = [0; 128];
        tokio::task::yield_now().await;
        for attempt in 0..3 {
            if attempt != 0 {
                assert_eq!(
                    receiver.try_recv_from(&mut buffer).unwrap_err().kind(),
                    io::ErrorKind::WouldBlock
                );
                tokio::time::advance(Duration::from_secs(1)).await;
                tokio::task::yield_now().await;
            }
            let (len, _) = recv_with_wall_deadline(&receiver, &mut buffer).await;
            assert_eq!(
                Frame::parse(&buffer[..len]).unwrap().payload,
                Payload::Control(Control::Disconnect)
            );
        }
        assert_eq!(state.handle().stats().sent_control_packets, 3);
    })
    .await;
}

#[tokio::test]
async fn initial_accept_is_counted_as_received_control() {
    super::test_support::wall_timeout(async {
        use super::test_support::RawPeer;
        use super::wire::{HandshakeFrame, HandshakeKind, HEADER};
        use super::UdpClientStream;
        use crate::protocols::protocol::ClientStream;

        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (client, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                UdpClientStream::connect(server.local_addr().unwrap()),
                async {
                    let mut buffer = [0; 256];
                    loop {
                        let (len, address) = server.recv_from(&mut buffer).await.unwrap();
                        let frame = HandshakeFrame::parse(&buffer[..len]).unwrap();
                        let response = RawPeer::respond(&buffer[..len]).unwrap();
                        server.send_to(&response, address).await.unwrap();
                        if frame.kind == HandshakeKind::Confirm {
                            break;
                        }
                    }
                }
            )
        })
        .await
        .unwrap();
        let handle = client.unwrap().transport();
        let stats = handle.stats();
        assert_eq!(stats.received_control_packets, 1);
        assert_eq!(stats.received_control_bytes, HEADER as u64);
    })
    .await;
}

#[tokio::test]
#[expect(
    clippy::significant_drop_tightening,
    reason = "The stream is consumed by into_split and its halves must remain live"
)]
async fn closing_server_read_counts_undelivered_raw_datagrams() {
    super::test_support::wall_timeout(async {
        use super::wire::Nonce;

        let listener =
            UdpNetworkListener::bind("127.0.0.1:0".parse().unwrap(), UdpOptions::DEFAULT)
                .await
                .unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = peer.local_addr().unwrap();
        let cookie = listener.issue_cookie(address, Nonce::from_bytes([14; 16]));
        let stream = listener
            .dispatch(
                &HandshakeFrame::confirm(cookie).encode(),
                address,
                Instant::now(),
            )
            .unwrap();
        let handle = stream.transport();
        let (read, _write) = stream.into_split().await.unwrap();
        for _ in 0..2 {
            listener.dispatch(
                &Frame::data(cookie.mac, &[7]).encode(),
                address,
                Instant::now(),
            );
        }
        drop(read);
        assert_eq!(handle.stats().dropped_closed_before_delivery, 2);
    })
    .await;
}

#[tokio::test(start_paused = true)]
#[expect(
    clippy::significant_drop_tightening,
    reason = "The accepted stream is split so its read half can cancel a paced write"
)]
async fn handle_updates_live_writer_rate_and_read_close_cancels_wait() {
    super::test_support::wall_timeout(async {
        use crate::protocols::protocol::PacketWriter;

        let listener = UdpNetworkListener::bind(
            "127.0.0.1:0".parse().unwrap(),
            UdpOptions {
                send_rate: Some(NonZeroU64::new(38).unwrap()),
                ..UdpOptions::DEFAULT
            },
        )
        .await
        .unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = peer.local_addr().unwrap();
        let cookie = listener.issue_cookie(address, Nonce::from_bytes([15; 16]));
        let stream = listener
            .dispatch(
                &HandshakeFrame::confirm(cookie).encode(),
                address,
                Instant::now(),
            )
            .unwrap();
        let handle = stream.transport();
        let (read, mut write) = stream.into_split().await.unwrap();
        let length = Length::default();
        write
            .send::<Vec<u8>, Vec<u8>, _, _>(vec![7], Arc::new(Raw), &length)
            .await
            .unwrap();

        let mut waiting =
            Box::pin(write.send::<Vec<u8>, Vec<u8>, _, _>(vec![8], Arc::new(Raw), &length));
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        tokio::time::advance(Duration::from_millis(900)).await;
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        handle.set_send_rate(Some(NonZeroU64::new(3_800).unwrap()));
        waiting.await.unwrap();
        assert_eq!(handle.stats().sent_data_packets, 2);
        assert_eq!(handle.stats().sent_data_bytes, 76);

        handle.set_send_rate(Some(NonZeroU64::new(1).unwrap()));
        let mut waiting =
            Box::pin(write.send::<Vec<u8>, Vec<u8>, _, _>(vec![9], Arc::new(Raw), &length));
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        drop(read);
        assert_eq!(
            waiting.await.unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
    })
    .await;
}

#[tokio::test]
async fn unsplit_server_stream_counts_undelivered_raw_datagrams() {
    super::test_support::wall_timeout(async {
        let listener =
            UdpNetworkListener::bind("127.0.0.1:0".parse().unwrap(), UdpOptions::DEFAULT)
                .await
                .unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = peer.local_addr().unwrap();
        let cookie = listener.issue_cookie(address, Nonce::from_bytes([16; 16]));
        let stream = listener
            .dispatch(
                &HandshakeFrame::confirm(cookie).encode(),
                address,
                Instant::now(),
            )
            .unwrap();
        let handle = stream.transport();
        for _ in 0..2 {
            listener.dispatch(
                &Frame::data(cookie.mac, &[7]).encode(),
                address,
                Instant::now(),
            );
        }
        drop(stream);
        assert_eq!(handle.stats().dropped_closed_before_delivery, 2);
    })
    .await;
}

#[tokio::test]
#[expect(
    clippy::significant_drop_tightening,
    reason = "The accepted stream is split to inspect queued datagram order and timestamps"
)]
async fn raw_drop_oldest_evicts_exact_datagram_and_preserves_timestamp() {
    super::test_support::wall_timeout(async {
        use crate::connection::OverflowPolicy;

        let listener = UdpNetworkListener::bind(
            "127.0.0.1:0".parse().unwrap(),
            UdpOptions {
                receive_queue_capacity: 2,
                receive_queue_bytes: 256,
                receive_queue_overflow: OverflowPolicy::DropOldest,
                ..UdpOptions::DEFAULT
            },
        )
        .await
        .unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = peer.local_addr().unwrap();
        let cookie = listener.issue_cookie(address, Nonce::from_bytes([17; 16]));
        let stream = listener
            .dispatch(
                &HandshakeFrame::confirm(cookie).encode(),
                address,
                Instant::now(),
            )
            .unwrap();
        let handle = stream.transport();
        let (mut read, _write) = stream.into_split().await.unwrap();
        let started = Instant::now();
        for (value, offset_ms) in [(1, 0), (2, 1), (3, 2)] {
            listener.dispatch(
                &Frame::data(cookie.mac, &[value]).encode(),
                address,
                started + Duration::from_millis(offset_ms),
            );
        }
        assert_eq!(handle.stats().dropped_raw_queue_evicted, 1);
        assert_eq!(handle.stats().dropped_raw_queue_full, 0);
        for (value, offset_ms) in [(2, 1), (3, 2)] {
            let (packet, received_at) = read
                .receive_with_timestamp::<Vec<u8>, Vec<u8>, _, _>(
                    Arc::new(Raw),
                    &Length::default(),
                    &ReceiveLimits::default(),
                )
                .await
                .unwrap();
            assert_eq!(packet, vec![value]);
            assert_eq!(received_at, started + Duration::from_millis(offset_ms));
        }
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn server_disconnect_attempts_consume_response_credits() {
    super::test_support::wall_timeout(async {
        use super::session::{Heartbeat, SessionState};
        use super::settings::ValidatedOptions;
        use super::wire::{Control, Cookie, Payload};

        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let sender = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let state = SessionState::new(
            Cookie::hello(Nonce::from_bytes([18; 16])),
            ValidatedOptions::new(UdpOptions::DEFAULT).unwrap(),
        );
        state.received();
        state.received();
        Heartbeat::new(
            sender,
            Some(receiver.local_addr().unwrap()),
            Arc::clone(&state),
        )
        .spawn();
        state.close();
        let mut buffer = [0; 128];
        tokio::task::yield_now().await;
        for attempt in 0..2 {
            if attempt != 0 {
                assert_eq!(
                    receiver.try_recv_from(&mut buffer).unwrap_err().kind(),
                    io::ErrorKind::WouldBlock
                );
                tokio::time::advance(Duration::from_secs(1)).await;
                tokio::task::yield_now().await;
            }
            let (len, _) = recv_with_wall_deadline(&receiver, &mut buffer).await;
            assert_eq!(
                Frame::parse(&buffer[..len]).unwrap().payload,
                Payload::Control(Control::Disconnect)
            );
        }
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            receiver.try_recv_from(&mut buffer).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(state.handle().stats().sent_control_packets, 2);
    })
    .await;
}

#[tokio::test]
#[expect(
    clippy::significant_drop_tightening,
    reason = "The accepted streams are split to verify byte-bounded queue contents"
)]
async fn raw_byte_budget_obeys_both_policies_without_eviction_for_oversized_item() {
    super::test_support::wall_timeout(async {
        use super::wire::HEADER;
        use crate::connection::OverflowPolicy;

        for policy in [OverflowPolicy::DropNewest, OverflowPolicy::DropOldest] {
            let listener = UdpNetworkListener::bind(
                "127.0.0.1:0".parse().unwrap(),
                UdpOptions {
                    receive_queue_capacity: 4,
                    receive_queue_bytes: (HEADER + 2) + (HEADER + 1),
                    receive_queue_overflow: policy,
                    ..UdpOptions::DEFAULT
                },
            )
            .await
            .unwrap();
            let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let address = peer.local_addr().unwrap();
            let cookie = listener.issue_cookie(address, Nonce::from_bytes([19; 16]));
            let stream = listener
                .dispatch(
                    &HandshakeFrame::confirm(cookie).encode(),
                    address,
                    Instant::now(),
                )
                .unwrap();
            let handle = stream.transport();
            let (mut read, _write) = stream.into_split().await.unwrap();
            let started = Instant::now();
            for (payload, offset_ms) in [(&[1, 1][..], 0), (&[2][..], 1), (&[3, 3][..], 2)] {
                listener.dispatch(
                    &Frame::data(cookie.mac, payload).encode(),
                    address,
                    started + Duration::from_millis(offset_ms),
                );
            }
            // This valid frame exceeds the queue byte budget by one byte. It must not
            // evict anything even under DropOldest.
            let too_large = vec![4; 41];
            listener.dispatch(
                &Frame::data(cookie.mac, &too_large).encode(),
                address,
                started + Duration::from_millis(3),
            );

            assert_eq!(read.queued_bytes(), 2 * HEADER + 3);
            let stats = handle.stats();
            assert_eq!(stats.received_data_packets, 4);
            assert_eq!(
                stats.received_data_bytes,
                (4 * HEADER + 2 + 1 + 2 + 41) as u64
            );
            let expected: &[(&[u8], u64)] = if policy == OverflowPolicy::DropNewest {
                assert_eq!(stats.dropped_raw_queue_full, 2);
                assert_eq!(stats.dropped_raw_queue_evicted, 0);
                &[(&[1, 1], 0), (&[2], 1)]
            } else {
                assert_eq!(stats.dropped_raw_queue_full, 1);
                assert_eq!(stats.dropped_raw_queue_evicted, 1);
                &[(&[2], 1), (&[3, 3], 2)]
            };
            for &(payload, offset_ms) in expected {
                let (packet, received_at) = read
                    .receive_with_timestamp::<Vec<u8>, Vec<u8>, _, _>(
                        Arc::new(Raw),
                        &Length::default(),
                        &ReceiveLimits::default(),
                    )
                    .await
                    .unwrap();
                assert_eq!(packet, payload);
                assert_eq!(received_at, started + Duration::from_millis(offset_ms));
            }
            assert_eq!(read.queued_bytes(), 0);
        }
    })
    .await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
#[expect(
    clippy::significant_drop_tightening,
    reason = "The accepted stream is split so the writer can recover after a socket error"
)]
async fn socket_send_error_is_counted_and_followed_by_healthy_send() {
    super::test_support::wall_timeout(async {
        use super::wire::Payload;
        use crate::protocols::protocol::PacketWriter;

        let listener =
            UdpNetworkListener::bind("127.0.0.1:0".parse().unwrap(), UdpOptions::DEFAULT)
                .await
                .unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = peer.local_addr().unwrap();
        let cookie = listener.issue_cookie(address, Nonce::from_bytes([20; 16]));
        let stream = listener
            .dispatch(
                &HandshakeFrame::confirm(cookie).encode(),
                address,
                Instant::now(),
            )
            .unwrap();
        let handle = stream.transport();
        let (_read, mut write) = stream.into_split().await.unwrap();
        let before = handle.stats().dropped_socket_send_error;
        let length = Length::default();

        // Linux rejects an IPv6 destination on the listener's IPv4 UDP socket.
        write.set_test_address("[::1]:9".parse().unwrap());
        write
            .send::<Vec<u8>, Vec<u8>, _, _>(vec![7], Arc::new(Raw), &length)
            .await
            .unwrap();
        assert_eq!(handle.stats().dropped_socket_send_error, before + 1);
        assert_eq!(handle.stats().sent_data_packets, 0);

        write.set_test_address(address);
        write
            .send::<Vec<u8>, Vec<u8>, _, _>(vec![8], Arc::new(Raw), &length)
            .await
            .unwrap();
        assert_eq!(handle.stats().dropped_socket_send_error, before + 1);
        assert_eq!(handle.stats().sent_data_packets, 1);
        assert_eq!(handle.stats().sent_data_bytes, 38);

        let mut buffer = [0; 128];
        for _ in 0..2 {
            let (len, _) = recv_with_wall_deadline(&peer, &mut buffer).await;
            if let Some(Frame {
                payload: Payload::Data(payload),
                ..
            }) = Frame::parse(&buffer[..len])
            {
                assert_eq!(payload, &[8]);
                return;
            }
        }
        panic!("healthy DATA datagram was not received");
    })
    .await;
}
