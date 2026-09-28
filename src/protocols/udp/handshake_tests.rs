use super::handshake::Handshake;
use super::settings::ValidatedOptions;
use super::wire::{Control, CookieJar, Frame, HandshakeFrame, HandshakeKind, Nonce};
use super::{UdpNetworkListener, UdpOptions};
use crate::{
    connection::ReceiveLimits,
    protocols::protocol::{NetworkStream, PacketReader},
    serializers::{packet_length_serializer::LittleEndian, serializer::Serializer},
};
use bevy::platform::time::Instant;
use std::{
    io::{self, ErrorKind},
    sync::Arc,
    time::Duration,
};
use tokio::net::UdpSocket;

struct BytesSerializer;
impl Serializer<Vec<u8>, Vec<u8>> for BytesSerializer {
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
async fn admitted_confirm_cannot_be_replayed_after_stream_closes() {
    super::test_support::wall_timeout(async {
        let listener =
            UdpNetworkListener::bind("127.0.0.1:0".parse().unwrap(), UdpOptions::DEFAULT)
                .await
                .unwrap();
        let address = "127.0.0.1:23456".parse().unwrap();
        let cookie = listener.issue_cookie(address, Nonce::from_bytes([1; 16]));
        let confirm = HandshakeFrame::confirm(cookie).encode();
        let stream = listener
            .dispatch(&confirm, address, Instant::now())
            .unwrap();
        drop(stream);

        assert!(listener
            .dispatch(&confirm, address, Instant::now())
            .is_none());
    })
    .await;
}

#[tokio::test]
async fn newer_admission_blocks_older_unadmitted_confirm() {
    super::test_support::wall_timeout(async {
        let listener =
            UdpNetworkListener::bind("127.0.0.1:0".parse().unwrap(), UdpOptions::DEFAULT)
                .await
                .unwrap();
        let address = "127.0.0.1:23457".parse().unwrap();
        let old = listener.issue_cookie(address, Nonce::from_bytes([1; 16]));
        let new = listener.issue_cookie(address, Nonce::from_bytes([2; 16]));
        let stream = listener
            .dispatch(
                &HandshakeFrame::confirm(new).encode(),
                address,
                Instant::now(),
            )
            .unwrap();
        drop(stream);
        assert!(listener
            .dispatch(
                &HandshakeFrame::confirm(old).encode(),
                address,
                Instant::now()
            )
            .is_none());
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn replay_capacity_fails_closed_until_cookie_expiry() {
    super::test_support::wall_timeout(async {
        let listener = UdpNetworkListener::bind(
            "127.0.0.1:0".parse().unwrap(),
            UdpOptions {
                max_replay_entries: 1,
                ..UdpOptions::DEFAULT
            },
        )
        .await
        .unwrap();
        let first_address = "127.0.0.1:23458".parse().unwrap();
        let second_address = "127.0.0.1:23459".parse().unwrap();
        let first = listener.issue_cookie(first_address, Nonce::from_bytes([1; 16]));
        let second = listener.issue_cookie(second_address, Nonce::from_bytes([2; 16]));
        let stream = listener
            .dispatch(
                &HandshakeFrame::confirm(first).encode(),
                first_address,
                Instant::now(),
            )
            .unwrap();
        drop(stream);
        assert!(listener
            .dispatch(
                &HandshakeFrame::confirm(second).encode(),
                second_address,
                Instant::now()
            )
            .is_none());

        tokio::time::advance(Duration::from_secs(60)).await;
        let fresh = listener.issue_cookie(second_address, Nonce::from_bytes([3; 16]));
        assert!(listener
            .dispatch(
                &HandshakeFrame::confirm(fresh).encode(),
                second_address,
                Instant::now()
            )
            .is_some());
    })
    .await;
}

#[tokio::test]
#[expect(
    clippy::significant_drop_tightening,
    reason = "The retained stream is the object whose DATA path this test checks"
)]
async fn failed_replacement_does_not_advance_replay_watermark() {
    super::test_support::wall_timeout(async {
        let listener = UdpNetworkListener::bind(
            "127.0.0.1:0".parse().unwrap(),
            UdpOptions {
                max_peers: 1,
                ..UdpOptions::DEFAULT
            },
        )
        .await
        .unwrap();
        let address = "127.0.0.1:23460".parse().unwrap();
        let old = listener.issue_cookie(address, Nonce::from_bytes([1; 16]));
        let old_stream = listener
            .dispatch(
                &HandshakeFrame::confirm(old).encode(),
                address,
                Instant::now(),
            )
            .unwrap();
        let next = listener.issue_cookie(address, Nonce::from_bytes([2; 16]));
        let confirm = HandshakeFrame::confirm(next).encode();
        assert!(listener
            .dispatch(&confirm, address, Instant::now())
            .is_none());
        // Slot exhaustion must leave the old registration and its DATA path intact.
        listener.dispatch(
            &Frame::data(old.mac, &[7]).encode(),
            address,
            Instant::now(),
        );
        let (mut read, write) = old_stream.into_split().await.unwrap();
        let packet: Vec<u8> = read
            .receive(
                Arc::new(BytesSerializer),
                &LittleEndian::<u32>::default(),
                &ReceiveLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(packet, [7]);
        drop(read);
        drop(write);
        assert!(listener
            .dispatch(&confirm, address, Instant::now())
            .is_some());
    })
    .await;
}

#[tokio::test]
async fn client_pins_first_challenge_and_retries_confirm_after_lost_accept() {
    super::test_support::wall_timeout(async {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(server.local_addr().unwrap()).await.unwrap();
        let options = ValidatedOptions::new(UdpOptions {
            connect_timeout: Duration::from_secs(1),
            initial_retry_interval: Duration::from_millis(20),
            max_retry_interval: Duration::from_millis(20),
            retry_jitter: Duration::ZERO,
            ..UdpOptions::DEFAULT
        })
        .unwrap();
        let server_task = async {
            let jar = CookieJar::new().unwrap();
            let mut buffer = [0; 128];
            let (len, address) = server.recv_from(&mut buffer).await.unwrap();
            let hello = HandshakeFrame::parse(&buffer[..len]).unwrap();
            assert_eq!(hello.kind, HandshakeKind::Hello);
            let first = jar.issue(address, hello.cookie.nonce);
            server
                .send_to(&HandshakeFrame::challenge(first).encode(), address)
                .await
                .unwrap();
            let (len, _) = server.recv_from(&mut buffer).await.unwrap();
            let confirm = HandshakeFrame::parse(&buffer[..len]).unwrap();
            assert_eq!(confirm.kind, HandshakeKind::Confirm);
            assert_eq!(confirm.cookie.mac, first.mac);
            let second = jar.issue(address, hello.cookie.nonce);
            server
                .send_to(&HandshakeFrame::challenge(second).encode(), address)
                .await
                .unwrap();
            let (len, _) = server.recv_from(&mut buffer).await.unwrap();
            let retry = HandshakeFrame::parse(&buffer[..len]).unwrap();
            assert_eq!(retry.kind, HandshakeKind::Confirm);
            assert_eq!(retry.cookie.mac, first.mac);
            server
                .send_to(&Control::Accept.encode(first.mac), address)
                .await
                .unwrap();
        };
        let (connected, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(Handshake::new(&client).connect(options), server_task)
        })
        .await
        .unwrap();
        assert!(connected.is_ok());
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn malformed_and_multiple_challenges_do_not_extend_deadline() {
    super::test_support::wall_timeout(async {
        let keep_clock_paused = tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        });
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(server.local_addr().unwrap()).await.unwrap();
        let options = ValidatedOptions::new(UdpOptions {
            connect_timeout: Duration::from_millis(80),
            initial_retry_interval: Duration::from_millis(20),
            max_retry_interval: Duration::from_millis(40),
            retry_jitter: Duration::ZERO,
            ..UdpOptions::DEFAULT
        })
        .unwrap();
        let client_task =
            tokio::spawn(async move { Handshake::new(&client).connect(options).await });
        let mut buffer = [0; 128];
        let (len, address) = server.recv_from(&mut buffer).await.unwrap();
        let hello = HandshakeFrame::parse(&buffer[..len]).unwrap();
        let jar = CookieJar::new().unwrap();
        let first = jar.issue(address, hello.cookie.nonce);
        server.send_to(&[0; 4], address).await.unwrap();
        server
            .send_to(&HandshakeFrame::challenge(first).encode(), address)
            .await
            .unwrap();
        let (len, _) = server.recv_from(&mut buffer).await.unwrap();
        assert_eq!(
            HandshakeFrame::parse(&buffer[..len]).unwrap().kind,
            HandshakeKind::Confirm
        );
        let second = jar.issue(address, hello.cookie.nonce);
        server
            .send_to(&HandshakeFrame::challenge(second).encode(), address)
            .await
            .unwrap();
        server.send_to(&[0; 4], address).await.unwrap();
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_millis(79)).await;
        assert!(!client_task.is_finished());
        tokio::time::advance(Duration::from_millis(1)).await;
        let error = client_task.await.unwrap().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::TimedOut);
        keep_clock_paused.abort();
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn hello_retry_uses_one_two_four_second_backoff_cap() {
    super::test_support::wall_timeout(async {
        let keep_clock_paused = tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        });
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(server.local_addr().unwrap()).await.unwrap();
        let options = ValidatedOptions::new(UdpOptions {
            connect_timeout: Duration::from_secs(12),
            retry_jitter: Duration::ZERO,
            ..UdpOptions::DEFAULT
        })
        .unwrap();
        let task = tokio::spawn(async move { Handshake::new(&client).connect(options).await });
        let mut buffer = [0; 128];
        let (len, _) = server.recv_from(&mut buffer).await.unwrap();
        assert_eq!(
            HandshakeFrame::parse(&buffer[..len]).unwrap().kind,
            HandshakeKind::Hello
        );
        for interval in [1, 2, 4, 4] {
            tokio::time::advance(
                Duration::from_secs(interval)
                    .checked_sub(Duration::from_millis(1))
                    .unwrap(),
            )
            .await;
            assert_eq!(
                server.try_recv_from(&mut buffer).unwrap_err().kind(),
                ErrorKind::WouldBlock
            );
            tokio::time::advance(Duration::from_millis(1)).await;
            for _ in 0..4 {
                tokio::task::yield_now().await;
            }
            let (len, _) = server.try_recv_from(&mut buffer).unwrap();
            assert_eq!(
                HandshakeFrame::parse(&buffer[..len]).unwrap().kind,
                HandshakeKind::Hello
            );
        }
        task.abort();
        keep_clock_paused.abort();
    })
    .await;
}

#[tokio::test]
async fn client_retries_hello_until_delayed_challenge_arrives() {
    super::test_support::wall_timeout(async {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(server.local_addr().unwrap()).await.unwrap();
        let options = ValidatedOptions::new(UdpOptions {
            connect_timeout: Duration::from_secs(1),
            initial_retry_interval: Duration::from_millis(20),
            max_retry_interval: Duration::from_millis(40),
            retry_jitter: Duration::ZERO,
            ..UdpOptions::DEFAULT
        })
        .unwrap();
        let server_task = async {
            let jar = CookieJar::new().unwrap();
            let mut buffer = [0; 128];
            let (len, address) = server.recv_from(&mut buffer).await.unwrap();
            let first = HandshakeFrame::parse(&buffer[..len]).unwrap();
            assert_eq!(first.kind, HandshakeKind::Hello);
            let (len, _) = server.recv_from(&mut buffer).await.unwrap();
            let retry = HandshakeFrame::parse(&buffer[..len]).unwrap();
            assert_eq!(retry.kind, HandshakeKind::Hello);
            assert_eq!(retry.cookie.nonce, first.cookie.nonce);
            let cookie = jar.issue(address, first.cookie.nonce);
            server
                .send_to(&HandshakeFrame::challenge(cookie).encode(), address)
                .await
                .unwrap();
            let (len, _) = server.recv_from(&mut buffer).await.unwrap();
            let confirm = HandshakeFrame::parse(&buffer[..len]).unwrap();
            assert_eq!(confirm.kind, HandshakeKind::Confirm);
            assert_eq!(confirm.cookie.mac, cookie.mac);
            server
                .send_to(&Control::Accept.encode(cookie.mac), address)
                .await
                .unwrap();
        };
        let (connected, ()) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(Handshake::new(&client).connect(options), server_task)
        })
        .await
        .unwrap();
        assert!(connected.is_ok());
    })
    .await;
}

#[test]
fn payload_helper_rejects_invalid_datagram_sizes() {
    assert_eq!(UdpOptions::DEFAULT.max_payload_size(), Some(1163));
    assert_eq!(
        UdpOptions {
            max_datagram_size: 36,
            ..UdpOptions::DEFAULT
        }
        .max_payload_size(),
        None
    );
    assert_eq!(
        UdpOptions {
            max_datagram_size: super::MAX_DATAGRAM_SIZE + 1,
            ..UdpOptions::DEFAULT
        }
        .max_payload_size(),
        None
    );
}

#[test]
fn invalid_retry_options_are_rejected() {
    for options in [
        UdpOptions {
            connect_timeout: Duration::ZERO,
            ..UdpOptions::DEFAULT
        },
        UdpOptions {
            initial_retry_interval: Duration::ZERO,
            ..UdpOptions::DEFAULT
        },
        UdpOptions {
            max_retry_interval: Duration::from_millis(1),
            ..UdpOptions::DEFAULT
        },
        UdpOptions {
            max_replay_entries: 0,
            ..UdpOptions::DEFAULT
        },
    ] {
        assert_eq!(
            ValidatedOptions::new(options).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
    }
}

#[tokio::test]
async fn replaced_admitted_confirm_cannot_reopen_after_both_streams_close() {
    super::test_support::wall_timeout(async {
        let listener =
            UdpNetworkListener::bind("127.0.0.1:0".parse().unwrap(), UdpOptions::DEFAULT)
                .await
                .unwrap();
        let address = "127.0.0.1:23461".parse().unwrap();
        let old = listener.issue_cookie(address, Nonce::from_bytes([1; 16]));
        let old_stream = listener
            .dispatch(
                &HandshakeFrame::confirm(old).encode(),
                address,
                Instant::now(),
            )
            .unwrap();
        let next = listener.issue_cookie(address, Nonce::from_bytes([2; 16]));
        let next_stream = listener
            .dispatch(
                &HandshakeFrame::confirm(next).encode(),
                address,
                Instant::now(),
            )
            .unwrap();
        drop(old_stream);
        drop(next_stream);
        assert!(listener
            .dispatch(
                &HandshakeFrame::confirm(old).encode(),
                address,
                Instant::now()
            )
            .is_none());
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn challenge_resets_confirm_retry_interval() {
    super::test_support::wall_timeout(async {
        let keep_clock_paused = tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        });
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client.connect(server.local_addr().unwrap()).await.unwrap();
        let options = ValidatedOptions::new(UdpOptions {
            connect_timeout: Duration::from_secs(5),
            retry_jitter: Duration::ZERO,
            ..UdpOptions::DEFAULT
        })
        .unwrap();
        let task = tokio::spawn(async move { Handshake::new(&client).connect(options).await });
        let mut buffer = [0; 128];
        let (len, address) = server.recv_from(&mut buffer).await.unwrap();
        let hello = HandshakeFrame::parse(&buffer[..len]).unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        let (len, _) = server.try_recv_from(&mut buffer).unwrap();
        assert_eq!(
            HandshakeFrame::parse(&buffer[..len]).unwrap().kind,
            HandshakeKind::Hello
        );
        let cookie = CookieJar::new().unwrap().issue(address, hello.cookie.nonce);
        server
            .send_to(&HandshakeFrame::challenge(cookie).encode(), address)
            .await
            .unwrap();
        let (len, _) = server.recv_from(&mut buffer).await.unwrap();
        assert_eq!(
            HandshakeFrame::parse(&buffer[..len]).unwrap().kind,
            HandshakeKind::Confirm
        );
        tokio::time::advance(Duration::from_millis(999)).await;
        assert_eq!(
            server.try_recv_from(&mut buffer).unwrap_err().kind(),
            ErrorKind::WouldBlock
        );
        tokio::time::advance(Duration::from_millis(1)).await;
        for _ in 0..4 {
            tokio::task::yield_now().await;
        }
        let (len, _) = server.try_recv_from(&mut buffer).unwrap();
        assert_eq!(
            HandshakeFrame::parse(&buffer[..len]).unwrap().kind,
            HandshakeKind::Confirm
        );
        task.abort();
        keep_clock_paused.abort();
    })
    .await;
}
