use super::session::HeartbeatTiming;
use super::settings::{ValidatedOptions, MAX_QUEUED_BYTES, MAX_QUEUED_DATAGRAMS};
use super::test_support::RawPeer;
use super::wire::*;
use super::*;
use crate::serializers::packet_length_serializer::LittleEndian;
use crate::{
    protocols::protocol::{
        ClientStream, Listener, NetworkStream, ReadStream, ReceiveError, WriteStream,
    },
    serializers::serializer::Serializer,
    Protocol,
};
use bevy::platform::time::Instant;
use std::{
    io::{self, ErrorKind},
    sync::Arc,
    time::Duration,
};
use tokio::{net::UdpSocket, sync::watch, time::Instant as Clock};
type Ls = LittleEndian<u32>;
struct Raw;
impl Serializer<Vec<u8>, Vec<u8>> for Raw {
    type EncodeError = io::Error;
    type DecodeError = io::Error;
    fn serialize(&self, packet: Vec<u8>) -> io::Result<Vec<u8>> {
        Ok(packet)
    }
    fn deserialize(&self, bytes: &[u8]) -> io::Result<Vec<u8>> {
        if bytes.first() == Some(&255) {
            Err(ErrorKind::InvalidData.into())
        } else {
            Ok(bytes.to_vec())
        }
    }
}
impl UdpReadHalf {
    async fn receive_raw(&mut self) -> Result<Vec<u8>, ReceiveError<io::Error, Ls>> {
        self.receive(Arc::new(Raw), &Ls::default()).await
    }
}
impl UdpWriteHalf {
    async fn send_raw(&mut self, packet: Vec<u8>) {
        self.send::<Vec<u8>, _, _, _>(packet, Arc::new(Raw), &Ls::default())
            .await
            .unwrap();
    }
}
struct ConnectedPair {
    client_read: UdpReadHalf,
    client_write: UdpWriteHalf,
    server_read: UdpReadHalf,
    server_write: UdpWriteHalf,
}
impl ConnectedPair {
    async fn new() -> Self {
        let listener = Arc::new(
            UdpProtocol::bind("127.0.0.1:0".parse().unwrap())
                .await
                .unwrap(),
        );
        let (client, server) = tokio::join!(
            UdpClientStream::connect(listener.address()),
            listener.accept()
        );
        let (cr, cw) = client.unwrap().into_split().await.unwrap();
        let (sr, sw) = server.unwrap().into_split().await.unwrap();
        tokio::spawn(async move { while listener.accept().await.is_ok() {} });
        Self {
            client_read: cr,
            client_write: cw,
            server_read: sr,
            server_write: sw,
        }
    }
}
struct AcceptedPeer {
    listener: UdpNetworkListener,
    peer: UdpSocket,
    cookie: Cookie,
    stream: UdpServerStream,
}
impl AcceptedPeer {
    async fn new(options: UdpOptions) -> Self {
        let listener = UdpNetworkListener::bind("127.0.0.1:0".parse().unwrap(), options)
            .await
            .unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = peer.local_addr().unwrap();
        let cookie = listener.issue_cookie(address, Nonce::from_bytes([9; 16]));
        let stream = listener
            .dispatch(
                &HandshakeFrame::confirm(cookie).encode(),
                address,
                Instant::now(),
            )
            .unwrap();
        Self {
            listener,
            peer,
            cookie,
            stream,
        }
    }
}

#[tokio::test]
async fn each_datagram_is_independent_including_empty_payloads() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let ConnectedPair {
            client_read: mut cr,
            client_write: mut cw,
            server_read: mut sr,
            server_write: mut sw,
        } = ConnectedPair::new().await;
        for packet in [vec![1], vec![], vec![255], vec![2, 3], vec![1]] {
            cw.send_raw(packet).await;
        }
        for expected in [vec![1], vec![], vec![2, 3], vec![1]] {
            assert_eq!(sr.receive_raw().await.unwrap(), expected);
        }
        sw.send_raw(vec![]).await;
        assert_eq!(cr.receive_raw().await.unwrap(), Vec::<u8>::new());
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn cookie_is_required_before_allocating_a_peer() {
    let listener = UdpProtocol::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let address = "127.0.0.1:12345".parse().unwrap();
    for bytes in [
        vec![],
        vec![1],
        HandshakeFrame::hello(Nonce::from_bytes([1; 16]))
            .encode()
            .to_vec(),
        HandshakeFrame::confirm(Cookie::hello(Nonce::from_bytes([1; 16])))
            .encode()
            .to_vec(),
    ] {
        assert!(listener.dispatch(&bytes, address, Instant::now()).is_none());
        assert_eq!(listener.peer_count(), 0);
    }
}

#[tokio::test(start_paused = true)]
async fn cookies_bind_address_nonce_generation_and_expiry() {
    let jar = CookieJar::new().unwrap();
    let address = "127.0.0.1:1234".parse().unwrap();
    let cookie = jar.issue(address, Nonce::from_bytes([7; 16]));
    assert!(jar.verify(address, &cookie));
    assert!(!jar.verify("127.0.0.1:1235".parse().unwrap(), &cookie));
    let mut tampered = cookie;
    let mut nonce = *tampered.nonce.as_bytes();
    nonce[0] ^= 1;
    tampered.nonce = Nonce::from_bytes(nonce);
    assert!(!jar.verify(address, &tampered));
    tampered = cookie;
    tampered.generation += 1;
    assert!(!jar.verify(address, &tampered));
    assert!(!CookieJar::new().unwrap().verify(address, &cookie));
    tokio::time::advance(Duration::from_secs(60)).await;
    assert!(!jar.verify(address, &cookie));
}

#[tokio::test]
#[expect(
    clippy::significant_drop_tightening,
    reason = "The replacement stream must stay alive while the old registration is dropped"
)]
async fn session_replacement_rejects_old_data_disconnect_and_confirmation() {
    let AcceptedPeer {
        listener,
        peer,
        cookie: old,
        stream: old_stream,
    } = AcceptedPeer::new(UdpOptions::DEFAULT).await;
    let address = peer.local_addr().unwrap();
    let new = listener.issue_cookie(address, Nonce::from_bytes([8; 16]));
    let stream = listener
        .dispatch(
            &HandshakeFrame::confirm(new).encode(),
            address,
            Instant::now(),
        )
        .unwrap();
    drop(old_stream); // Its registration must not remove the replacement.
    let (mut read, _write) = stream.into_split().await.unwrap();
    for bytes in [
        Frame::data(old.mac, &[1]).encode(),
        Control::Disconnect.encode(old.mac).to_vec(),
        HandshakeFrame::confirm(old).encode().to_vec(),
    ] {
        assert!(listener.dispatch(&bytes, address, Instant::now()).is_none());
    }
    assert!(!read.session().is_closed());
    assert_eq!(listener.peer_count(), 1);
    let received_at = Instant::now();
    listener.dispatch(&Frame::data(new.mac, &[7]).encode(), address, received_at);
    let (packet, timestamp) = read
        .receive_with_timestamp::<_, Vec<u8>, _, _>(Arc::new(Raw), &Ls::default())
        .await
        .unwrap();
    assert_eq!(packet, [7]);
    assert_eq!(timestamp, received_at);
    drop(read);
    assert_eq!(listener.peer_count(), 0);
}

#[tokio::test]
async fn duplicate_confirmations_do_not_allocate_another_peer() {
    let AcceptedPeer {
        listener,
        peer,
        cookie,
        stream: _stream,
    } = AcceptedPeer::new(UdpOptions::DEFAULT).await;
    assert!(listener
        .dispatch(
            &HandshakeFrame::confirm(cookie).encode(),
            peer.local_addr().unwrap(),
            Instant::now()
        )
        .is_none());
    assert_eq!(listener.peer_count(), 1);
}

#[tokio::test]
async fn peer_cap_includes_unsplit_connections() {
    let AcceptedPeer {
        listener,
        peer: _peer,
        cookie: _cookie,
        stream,
    } = AcceptedPeer::new(UdpOptions {
        max_peers: 1,
        ..UdpOptions::DEFAULT
    })
    .await;
    let address = "127.0.0.1:1234".parse().unwrap();
    let cookie = listener.issue_cookie(address, Nonce::from_bytes([1; 16]));
    assert!(listener
        .dispatch(
            &HandshakeFrame::confirm(cookie).encode(),
            address,
            Instant::now()
        )
        .is_none());
    assert_eq!(listener.peer_count(), 1);
    drop(stream);
    assert!(listener
        .dispatch(
            &HandshakeFrame::confirm(cookie).encode(),
            address,
            Instant::now()
        )
        .is_some());
}

#[tokio::test(start_paused = true)]
async fn handshake_rate_limit_recovers_without_allocating_pending_state() {
    let listener = UdpNetworkListener::bind(
        "127.0.0.1:0".parse().unwrap(),
        UdpOptions {
            max_handshake_packets_per_second: 1,
            ..UdpOptions::DEFAULT
        },
    )
    .await
    .unwrap();
    let address = "127.0.0.1:1234".parse().unwrap();
    listener.dispatch(
        &HandshakeFrame::hello(Nonce::from_bytes([0; 16])).encode(),
        address,
        Instant::now(),
    );
    let cookie = listener.issue_cookie(address, Nonce::from_bytes([0; 16]));
    assert!(listener
        .dispatch(
            &HandshakeFrame::confirm(cookie).encode(),
            address,
            Instant::now()
        )
        .is_none());
    assert_eq!(listener.peer_count(), 0);
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(listener
        .dispatch(
            &HandshakeFrame::confirm(cookie).encode(),
            address,
            Instant::now()
        )
        .is_some());
}

#[tokio::test]
async fn queue_limits_and_control_packets_under_overload() {
    let AcceptedPeer {
        listener,
        peer,
        cookie,
        stream,
    } = AcceptedPeer::new(UdpOptions::DEFAULT).await;
    let address = peer.local_addr().unwrap();
    let (mut read, _write) = stream.into_split().await.unwrap();
    for _ in 0..=MAX_QUEUED_DATAGRAMS {
        listener.dispatch(
            &Frame::data(cookie.mac, &[7]).encode(),
            address,
            Instant::now(),
        );
    }
    assert_eq!(
        read.session().queued_bytes(),
        (HEADER + 1) * MAX_QUEUED_DATAGRAMS
    );
    assert_eq!(read.receive_raw().await.unwrap(), [7]);
    // Disconnect bypasses the saturated application queue.
    listener.dispatch(
        &Control::Disconnect.encode(cookie.mac),
        address,
        Instant::now(),
    );
    assert!(
        matches!(read.receive_raw().await, Err(ReceiveError::Io(err)) if err.kind() == ErrorKind::ConnectionAborted)
    );
}

#[tokio::test]
async fn queue_byte_budget_is_bounded() {
    let AcceptedPeer {
        listener,
        peer,
        cookie,
        stream,
    } = AcceptedPeer::new(UdpOptions::DEFAULT).await;
    let address = peer.local_addr().unwrap();
    let bytes = Frame::data(cookie.mac, &vec![7; MAX_DATAGRAM_SIZE - HEADER]).encode();
    for _ in 0..100 {
        listener.dispatch(&bytes, address, Instant::now());
    }
    let (mut read, _write) = stream.into_split().await.unwrap();
    let expected = MAX_QUEUED_BYTES / bytes.len() * bytes.len();
    assert_eq!(read.session().queued_bytes(), expected);
    read.receive_raw().await.unwrap();
    assert_eq!(read.session().queued_bytes(), expected - bytes.len());
}

#[tokio::test(start_paused = true)]
async fn idle_deadline_tracks_reception_and_setting_changes() {
    let AcceptedPeer {
        listener,
        peer,
        cookie,
        stream,
    } = AcceptedPeer::new(UdpOptions::DEFAULT).await;
    let address = peer.local_addr().unwrap();
    let (mut read, _write) = stream.into_split().await.unwrap();
    let (settings, timeout) = watch::channel(Duration::MAX);
    read.set_idle_timeout(timeout);
    let mut waiting = Box::pin(read.receive_raw());
    assert!(futures::poll!(waiting.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(20)).await;
    listener.dispatch(
        &Control::Keepalive.encode(cookie.mac),
        address,
        Instant::now(),
    );
    settings.send_replace(Duration::from_secs(2));
    assert!(futures::poll!(waiting.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(
        matches!(waiting.await, Err(ReceiveError::Io(err)) if err.kind() == ErrorKind::TimedOut)
    );
}

#[tokio::test(start_paused = true)]
async fn invalid_or_stale_packets_do_not_extend_idle_timeout() {
    let AcceptedPeer {
        listener,
        peer,
        cookie: _cookie,
        stream,
    } = AcceptedPeer::new(UdpOptions::DEFAULT).await;
    let address = peer.local_addr().unwrap();
    let (mut read, _write) = stream.into_split().await.unwrap();
    let mut waiting = Box::pin(read.receive_raw());
    assert!(futures::poll!(waiting.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(9)).await;
    listener.dispatch(
        &Frame::data(Session::from_bytes([0; 32]), &[7]).encode(),
        address,
        Instant::now(),
    );
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(
        matches!(waiting.await, Err(ReceiveError::Io(err)) if err.kind() == ErrorKind::TimedOut)
    );
}

#[tokio::test]
async fn dropping_read_half_closes_writer_and_remote() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let ConnectedPair { client_read: cr, client_write: mut cw, server_read: mut sr, server_write: _sw } = ConnectedPair::new().await;
        drop(cr);
        assert!(cw.send::<Vec<u8>, _, _, _>(vec![1], Arc::new(Raw), &Ls::default()).await.is_err());
        assert!(matches!(sr.receive_raw().await, Err(ReceiveError::Io(err)) if err.kind() == ErrorKind::ConnectionAborted));
    }).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn connect_times_out_without_a_response() {
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let started = Clock::now();
    let result = UdpClientStream::connect(server.local_addr().unwrap()).await;
    assert!(matches!(result, Err(err) if err.kind() == ErrorKind::TimedOut));
    assert_eq!(started.elapsed(), CONNECT_TIMEOUT);
}

#[tokio::test]
async fn handshake_retries_both_legs_and_preserves_early_data() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap();
        let (result, ()) = tokio::join!(UdpClientStream::connect(address), async {
            let mut buffer = [0; 256];
            let (mut hellos, mut confirms) = (0, 0);
            loop {
                let (len, address) = server.recv_from(&mut buffer).await.unwrap();
                let Some(HandshakeFrame { kind, cookie }) = HandshakeFrame::parse(&buffer[..len])
                else {
                    continue;
                };
                if kind == HandshakeKind::Hello {
                    hellos += 1;
                    if hellos == 1 {
                        continue;
                    }
                    server
                        .send_to(&RawPeer::respond(&buffer[..len]).unwrap(), address)
                        .await
                        .unwrap();
                } else if kind == HandshakeKind::Confirm {
                    confirms += 1;
                    if confirms == 1 {
                        continue;
                    }
                    server
                        .send_to(&Frame::data(cookie.mac, &[7]).encode(), address)
                        .await
                        .unwrap();
                    break;
                }
            }
            assert!(hellos >= 2 && confirms >= 2);
        });
        let (mut read, _write) = result.unwrap().into_split().await.unwrap();
        assert_eq!(read.receive_raw().await.unwrap(), [7]);
    })
    .await
    .unwrap();
}

#[test]
fn invalid_frames_are_rejected() {
    for bytes in [
        vec![],
        vec![1],
        {
            let mut bytes = Control::Keepalive
                .encode(Session::from_bytes([1; 32]))
                .to_vec();
            bytes.push(9);
            bytes
        },
        {
            let mut bytes = vec![1; 37];
            bytes[..5].copy_from_slice(b"SLN2\x09");
            bytes
        },
        vec![1; MAX_DATAGRAM_SIZE + 1],
    ] {
        assert!(Frame::parse(&bytes).is_none());
    }
    assert!(Frame::parse(&Frame::data(Session::from_bytes([1; 32]), &[]).encode()).is_some());
}

#[cfg(any(feature = "client", feature = "server"))]
#[test]
fn idle_timeout_settings_are_per_app() {
    use bevy::prelude::App;
    let mut first = App::new();
    first.insert_resource(UdpIdleTimeout(Duration::MAX));
    let a = IdleTimeoutSettings::install(&mut first);
    first.update();
    let mut second = App::new();
    let b = IdleTimeoutSettings::install(&mut second);
    second.update();
    assert_eq!(*a.borrow(), Duration::MAX);
    assert_eq!(*b.borrow(), UdpIdleTimeout::default().0);
    second.insert_resource(UdpIdleTimeout(Duration::from_secs(3)));
    second.update();
    assert_eq!(*b.borrow(), Duration::from_secs(3));
    second.world_mut().remove_resource::<UdpIdleTimeout>();
    second.update();
    assert_eq!(*b.borrow(), UdpIdleTimeout::default().0);
}

#[tokio::test]
async fn outgoing_size_budget_drops_only_oversized_packets() {
    let ConnectedPair {
        client_read: _cr,
        client_write: mut cw,
        server_read: mut sr,
        server_write: _sw,
    } = ConnectedPair::new().await;
    let payload_limit = UdpOptions::DEFAULT.max_datagram_size - HEADER;
    cw.send_raw(vec![7; payload_limit + 1]).await;
    assert_eq!(cw.dropped_oversized_packets(), 1);
    cw.send_raw(vec![7; payload_limit]).await;
    assert_eq!(sr.receive_raw().await.unwrap(), vec![7; payload_limit]);
    cw.send_raw(vec![]).await;
    assert!(sr.receive_raw().await.unwrap().is_empty());
}

#[tokio::test]
async fn configured_protocol_applies_options_on_both_ends() {
    struct Small;
    impl UdpConfig for Small {
        const OPTIONS: UdpOptions = UdpOptions {
            max_datagram_size: HEADER + 2,
            ..UdpOptions::DEFAULT
        };
    }
    let listener = Arc::new(
        ConfiguredUdpProtocol::<Small>::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap(),
    );
    let (client, server) = tokio::join!(
        ConfiguredUdpProtocol::<Small>::connect_to_server(listener.address()),
        listener.accept()
    );
    let (mut cr, mut cw) = client.unwrap().into_split().await.unwrap();
    let (mut sr, mut sw) = server.unwrap().into_split().await.unwrap();
    tokio::spawn(async move { while listener.accept().await.is_ok() {} });
    for write in [&mut cw, &mut sw] {
        write.send_raw(vec![7; 3]).await;
        assert_eq!(write.dropped_oversized_packets(), 1);
        write.send_raw(vec![8; 2]).await;
    }
    assert_eq!(cr.receive_raw().await.unwrap(), [8, 8]);
    assert_eq!(sr.receive_raw().await.unwrap(), [8, 8]);
}

#[test]
fn invalid_udp_options_are_rejected() {
    for options in [
        UdpOptions {
            max_datagram_size: HEADER - 1,
            ..UdpOptions::DEFAULT
        },
        UdpOptions {
            max_datagram_size: MAX_DATAGRAM_SIZE + 1,
            ..UdpOptions::DEFAULT
        },
        UdpOptions {
            heartbeat_interval: Duration::ZERO,
            ..UdpOptions::DEFAULT
        },
        UdpOptions {
            heartbeat_jitter: Duration::MAX,
            ..UdpOptions::DEFAULT
        },
    ] {
        assert!(ValidatedOptions::new(options).is_err());
    }
}

#[test]
fn heartbeat_jitter_stays_inside_its_budget_and_varies() {
    let options = UdpOptions::DEFAULT;
    let mut timing = HeartbeatTiming::new(ValidatedOptions::new(options).unwrap(), 42);
    let delays: Vec<_> = (0..100).map(|_| timing.next_delay()).collect();
    assert!(delays
        .iter()
        .all(|delay| *delay >= options.heartbeat_interval
            && *delay <= options.heartbeat_interval + options.heartbeat_jitter));
    assert!(delays.windows(2).any(|pair| pair[0] != pair[1]));
}

struct RawServer {
    server: UdpSocket,
    read: UdpReadHalf,
    write: UdpWriteHalf,
}
impl RawServer {
    async fn new(options: UdpOptions) -> Self {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (client, ()) = tokio::join!(
            UdpClientStream::connect_with_options(server.local_addr().unwrap(), options),
            async {
                let mut buffer = [0; 256];
                loop {
                    let (len, peer) = server.recv_from(&mut buffer).await.unwrap();
                    let response = RawPeer::respond(&buffer[..len]).unwrap();
                    server.send_to(&response, peer).await.unwrap();
                    if buffer[4] == 6 {
                        break;
                    }
                }
            }
        );
        let (read, write) = client.unwrap().into_split().await.unwrap();
        Self {
            server,
            read,
            write,
        }
    }
}

#[tokio::test(start_paused = true)]
async fn application_traffic_suppresses_heartbeats_and_idle_resumes_them() {
    let RawServer {
        server,
        read: _read,
        mut write,
    } = RawServer::new(UdpOptions::DEFAULT).await;
    let mut buffer = [0; 256];
    for _ in 0..8 {
        tokio::time::advance(Duration::from_millis(500)).await;
        write.send_raw(vec![7]).await;
        let len = server.recv(&mut buffer).await.unwrap();
        assert_eq!(
            Frame::parse(&buffer[..len]).unwrap().payload,
            Payload::Data(&[7])
        );
    }
    let last_data = Clock::now();
    let len = server.recv(&mut buffer).await.unwrap();
    assert_eq!(
        Frame::parse(&buffer[..len]).unwrap().payload,
        Payload::Control(Control::Keepalive)
    );
    assert!(last_data.elapsed() >= KEEPALIVE_INTERVAL);
    assert!(last_data.elapsed() <= (KEEPALIVE_INTERVAL + UdpOptions::DEFAULT.heartbeat_jitter) * 2);
}

#[tokio::test(start_paused = true)]
async fn idle_sessions_stay_alive_with_heartbeats() {
    let ConnectedPair {
        client_read: mut cr,
        client_write: _cw,
        server_read: mut sr,
        server_write: mut sw,
    } = ConnectedPair::new().await;
    let server = tokio::spawn(async move { sr.receive_raw().await });
    let client = tokio::spawn(async move { cr.receive_raw().await });
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert!(!server.is_finished());
    assert!(!client.is_finished());
    sw.send_raw(vec![7]).await;
    assert_eq!(client.await.unwrap().unwrap(), [7]);
    server.abort();
}

#[tokio::test]
async fn client_ignores_stale_disconnect_and_malformed_frames() {
    let RawServer {
        server,
        mut read,
        write,
    } = RawServer::new(UdpOptions::DEFAULT).await;
    let address = write.local_addr();
    for bytes in [
        Control::Disconnect
            .encode(Session::from_bytes([0; 32]))
            .to_vec(),
        vec![1],
        {
            let mut bytes = Control::Keepalive.encode(write.session().id()).to_vec();
            bytes.push(1);
            bytes
        },
        Frame::data(write.session().id(), &[255]).encode(),
        Frame::data(write.session().id(), &[7]).encode(),
    ] {
        server.send_to(&bytes, address).await.unwrap();
    }
    assert_eq!(read.receive_raw().await.unwrap(), [7]);
}

#[tokio::test]
async fn refusal_is_reported_only_for_the_selected_session() {
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (result, ()) = tokio::join!(
        UdpClientStream::connect(server.local_addr().unwrap()),
        async {
            let mut buffer = [0; 256];
            let (len, peer) = server.recv_from(&mut buffer).await.unwrap();
            server
                .send_to(
                    &Control::Disconnect.encode(Session::from_bytes([0; 32])),
                    peer,
                )
                .await
                .unwrap();
            server
                .send_to(&RawPeer::respond(&buffer[..len]).unwrap(), peer)
                .await
                .unwrap();
            let (len, peer) = server.recv_from(&mut buffer).await.unwrap();
            let HandshakeFrame { kind, cookie } = HandshakeFrame::parse(&buffer[..len]).unwrap();
            assert_eq!(kind, HandshakeKind::Confirm);
            server
                .send_to(&Control::Disconnect.encode(cookie.mac), peer)
                .await
                .unwrap();
        }
    );
    assert!(matches!(result, Err(err) if err.kind() == ErrorKind::ConnectionRefused));
}

#[tokio::test]
async fn replacement_registrations_also_consume_peer_slots() {
    let AcceptedPeer {
        listener,
        peer,
        cookie: _cookie,
        stream: old,
    } = AcceptedPeer::new(UdpOptions {
        max_peers: 2,
        ..UdpOptions::DEFAULT
    })
    .await;
    let address = peer.local_addr().unwrap();
    let second = listener.issue_cookie(address, Nonce::from_bytes([2; 16]));
    let replacement = listener
        .dispatch(
            &HandshakeFrame::confirm(second).encode(),
            address,
            Instant::now(),
        )
        .unwrap();
    let third = listener.issue_cookie(address, Nonce::from_bytes([3; 16]));
    assert!(listener
        .dispatch(
            &HandshakeFrame::confirm(third).encode(),
            address,
            Instant::now()
        )
        .is_none());
    drop(old);
    assert!(listener
        .dispatch(
            &HandshakeFrame::confirm(third).encode(),
            address,
            Instant::now()
        )
        .is_some());
    drop(replacement);
}

#[tokio::test]
async fn receiving_disconnect_closes_a_retained_read_half_and_writer() {
    let RawServer {
        server,
        mut read,
        mut write,
    } = RawServer::new(UdpOptions::DEFAULT).await;
    server
        .send_to(
            &Control::Disconnect.encode(write.session().id()),
            write.local_addr(),
        )
        .await
        .unwrap();
    assert!(
        matches!(read.receive_raw().await, Err(ReceiveError::Io(err)) if err.kind() == ErrorKind::ConnectionAborted)
    );
    assert!(write
        .send::<Vec<u8>, _, _, _>(vec![7], Arc::new(Raw), &Ls::default())
        .await
        .is_err());
}
