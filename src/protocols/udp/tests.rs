use super::*;
use crate::packet_length_serializer::LittleEndian;
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
async fn receive(read: &mut UdpReadHalf) -> Result<Vec<u8>, ReceiveError<io::Error, Ls>> {
    read.receive(Arc::new(Raw), &Ls::default()).await
}
async fn send(write: &mut UdpWriteHalf, packet: Vec<u8>) {
    write
        .send::<Vec<u8>, _, _, _>(packet, Arc::new(Raw), &Ls::default())
        .await
        .unwrap();
}
async fn pair() -> (UdpReadHalf, UdpWriteHalf, UdpReadHalf, UdpWriteHalf) {
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
    (cr, cw, sr, sw)
}
async fn fixture(options: UdpOptions) -> (UdpNetworkListener, UdpSocket, Cookie, UdpServerStream) {
    let listener = UdpNetworkListener::bind("127.0.0.1:0".parse().unwrap(), options)
        .await
        .unwrap();
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = peer.local_addr().unwrap();
    let cookie = listener.cookies.issue(address, [9; 16]);
    let stream = listener
        .dispatch(&cookie.encode(CONFIRM), address, Instant::now())
        .unwrap();
    (listener, peer, cookie, stream)
}

#[tokio::test]
async fn each_datagram_is_independent_including_empty_payloads() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (mut cr, mut cw, mut sr, mut sw) = pair().await;
        for packet in [vec![1], vec![], vec![255], vec![2, 3], vec![1]] {
            send(&mut cw, packet).await;
        }
        for expected in [vec![1], vec![], vec![2, 3], vec![1]] {
            assert_eq!(receive(&mut sr).await.unwrap(), expected);
        }
        send(&mut sw, vec![]).await;
        assert_eq!(receive(&mut cr).await.unwrap(), Vec::<u8>::new());
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
        Cookie::hello([1; 16]).encode(HELLO).to_vec(),
        Cookie::hello([1; 16]).encode(CONFIRM).to_vec(),
    ] {
        assert!(listener.dispatch(&bytes, address, Instant::now()).is_none());
        assert!(listener.peers.lock().unwrap().is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn cookies_bind_address_nonce_generation_and_expiry() {
    let jar = CookieJar::new().unwrap();
    let address = "127.0.0.1:1234".parse().unwrap();
    let cookie = jar.issue(address, [7; 16]);
    assert!(jar.verify(address, &cookie));
    assert!(!jar.verify("127.0.0.1:1235".parse().unwrap(), &cookie));
    let mut tampered = cookie;
    tampered.nonce[0] ^= 1;
    assert!(!jar.verify(address, &tampered));
    tampered = cookie;
    tampered.generation += 1;
    assert!(!jar.verify(address, &tampered));
    assert!(!CookieJar::new().unwrap().verify(address, &cookie));
    tokio::time::advance(Duration::from_secs(60)).await;
    assert!(!jar.verify(address, &cookie));
}

#[tokio::test]
async fn session_replacement_rejects_old_data_disconnect_and_confirmation() {
    let (listener, peer, old, old_stream) = fixture(UdpOptions::DEFAULT).await;
    let address = peer.local_addr().unwrap();
    let new = listener.cookies.issue(address, [8; 16]);
    let stream = listener
        .dispatch(&new.encode(CONFIRM), address, Instant::now())
        .unwrap();
    drop(old_stream); // Its registration must not remove the replacement.
    let (mut read, _write) = stream.into_split().await.unwrap();
    for bytes in [
        frame(DATA, &old.mac, &[1]),
        control(DISCONNECT, &old.mac).to_vec(),
        old.encode(CONFIRM).to_vec(),
    ] {
        assert!(listener.dispatch(&bytes, address, Instant::now()).is_none());
    }
    assert!(!read.state.closed.is_cancelled());
    assert_eq!(listener.peers.lock().unwrap().len(), 1);
    let received_at = Instant::now();
    listener.dispatch(&frame(DATA, &new.mac, &[7]), address, received_at);
    let (packet, timestamp) = read
        .receive_with_timestamp::<_, Vec<u8>, _, _>(Arc::new(Raw), &Ls::default())
        .await
        .unwrap();
    assert_eq!(packet, [7]);
    assert_eq!(timestamp, received_at);
    drop(read);
    assert!(listener.peers.lock().unwrap().is_empty());
}

#[tokio::test]
async fn duplicate_confirmations_do_not_allocate_another_peer() {
    let (listener, peer, cookie, _stream) = fixture(UdpOptions::DEFAULT).await;
    assert!(listener
        .dispatch(
            &cookie.encode(CONFIRM),
            peer.local_addr().unwrap(),
            Instant::now()
        )
        .is_none());
    assert_eq!(listener.peers.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn peer_cap_includes_unsplit_connections() {
    let (listener, _peer, _cookie, stream) = fixture(UdpOptions {
        max_peers: 1,
        ..UdpOptions::DEFAULT
    })
    .await;
    let address = "127.0.0.1:1234".parse().unwrap();
    let cookie = listener.cookies.issue(address, [1; 16]);
    assert!(listener
        .dispatch(&cookie.encode(CONFIRM), address, Instant::now())
        .is_none());
    assert_eq!(listener.peers.lock().unwrap().len(), 1);
    drop(stream);
    assert!(listener
        .dispatch(&cookie.encode(CONFIRM), address, Instant::now())
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
        &Cookie::hello([0; 16]).encode(HELLO),
        address,
        Instant::now(),
    );
    let cookie = listener.cookies.issue(address, [0; 16]);
    assert!(listener
        .dispatch(&cookie.encode(CONFIRM), address, Instant::now())
        .is_none());
    assert!(listener.peers.lock().unwrap().is_empty());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(listener
        .dispatch(&cookie.encode(CONFIRM), address, Instant::now())
        .is_some());
}

#[tokio::test]
async fn queue_limits_and_control_packets_under_overload() {
    let (listener, peer, cookie, stream) = fixture(UdpOptions::DEFAULT).await;
    let address = peer.local_addr().unwrap();
    let (mut read, _write) = stream.into_split().await.unwrap();
    for _ in 0..MAX_QUEUED_DATAGRAMS + 1 {
        listener.dispatch(&frame(DATA, &cookie.mac, &[7]), address, Instant::now());
    }
    assert_eq!(
        read.state.queued_bytes.load(Ordering::Relaxed),
        (HEADER + 1) * MAX_QUEUED_DATAGRAMS
    );
    assert_eq!(receive(&mut read).await.unwrap(), [7]);
    // Disconnect bypasses the saturated application queue.
    listener.dispatch(&control(DISCONNECT, &cookie.mac), address, Instant::now());
    assert!(
        matches!(receive(&mut read).await, Err(ReceiveError::Io(err)) if err.kind() == ErrorKind::ConnectionAborted)
    );
}

#[tokio::test]
async fn queue_byte_budget_is_bounded() {
    let (listener, peer, cookie, stream) = fixture(UdpOptions::DEFAULT).await;
    let address = peer.local_addr().unwrap();
    let bytes = frame(DATA, &cookie.mac, &vec![7; MAX_DATAGRAM_SIZE - HEADER]);
    for _ in 0..100 {
        listener.dispatch(&bytes, address, Instant::now());
    }
    let (mut read, _write) = stream.into_split().await.unwrap();
    let expected = MAX_QUEUED_BYTES / bytes.len() * bytes.len();
    assert_eq!(read.state.queued_bytes.load(Ordering::Relaxed), expected);
    receive(&mut read).await.unwrap();
    assert_eq!(
        read.state.queued_bytes.load(Ordering::Relaxed),
        expected - bytes.len()
    );
}

#[tokio::test(start_paused = true)]
async fn idle_deadline_tracks_reception_and_setting_changes() {
    let (listener, peer, cookie, stream) = fixture(UdpOptions::DEFAULT).await;
    let address = peer.local_addr().unwrap();
    let (mut read, _write) = stream.into_split().await.unwrap();
    let (settings, timeout) = watch::channel(Duration::MAX);
    read.set_idle_timeout(timeout);
    let mut waiting = Box::pin(receive(&mut read));
    assert!(futures::poll!(waiting.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(20)).await;
    listener.dispatch(&control(KEEPALIVE, &cookie.mac), address, Instant::now());
    settings.send_replace(Duration::from_secs(2));
    assert!(futures::poll!(waiting.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(
        matches!(waiting.await, Err(ReceiveError::Io(err)) if err.kind() == ErrorKind::TimedOut)
    );
}

#[tokio::test(start_paused = true)]
async fn invalid_or_stale_packets_do_not_extend_idle_timeout() {
    let (listener, peer, _cookie, stream) = fixture(UdpOptions::DEFAULT).await;
    let address = peer.local_addr().unwrap();
    let (mut read, _write) = stream.into_split().await.unwrap();
    let mut waiting = Box::pin(receive(&mut read));
    assert!(futures::poll!(waiting.as_mut()).is_pending());
    tokio::time::advance(Duration::from_secs(9)).await;
    listener.dispatch(&frame(DATA, &[0; 32], &[7]), address, Instant::now());
    tokio::time::advance(Duration::from_secs(1)).await;
    assert!(
        matches!(waiting.await, Err(ReceiveError::Io(err)) if err.kind() == ErrorKind::TimedOut)
    );
}

#[tokio::test]
async fn dropping_read_half_closes_writer_and_remote() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let (cr, mut cw, mut sr, _sw) = pair().await;
        drop(cr);
        assert!(cw.send::<Vec<u8>, _, _, _>(vec![1], Arc::new(Raw), &Ls::default()).await.is_err());
        assert!(matches!(receive(&mut sr).await, Err(ReceiveError::Io(err)) if err.kind() == ErrorKind::ConnectionAborted));
    }).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn connect_times_out_without_an_answer() {
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
                let Some((tag, cookie)) = Cookie::parse(&buffer[..len]) else {
                    continue;
                };
                if tag == HELLO {
                    hellos += 1;
                    if hellos == 1 {
                        continue;
                    }
                    server
                        .send_to(&test_answer(&buffer[..len]).unwrap(), address)
                        .await
                        .unwrap();
                } else if tag == CONFIRM {
                    confirms += 1;
                    if confirms == 1 {
                        continue;
                    }
                    server
                        .send_to(&frame(DATA, &cookie.mac, &[7]), address)
                        .await
                        .unwrap();
                    break;
                }
            }
            assert!(hellos >= 2 && confirms >= 2);
        });
        let (mut read, _write) = result.unwrap().into_split().await.unwrap();
        assert_eq!(receive(&mut read).await.unwrap(), [7]);
    })
    .await
    .unwrap();
}

#[test]
fn invalid_frames_are_rejected() {
    for bytes in [
        vec![],
        vec![1],
        frame(KEEPALIVE, &[1; 32], &[9]),
        frame(9, &[1; 32], &[]),
        vec![1; MAX_DATAGRAM_SIZE + 1],
    ] {
        assert!(parse_frame(&bytes).is_none());
    }
    assert!(parse_frame(&frame(DATA, &[1; 32], &[])).is_some());
}

#[cfg(any(feature = "client", feature = "server"))]
#[test]
fn idle_timeout_settings_are_per_app() {
    use bevy::prelude::App;
    let mut first = App::new();
    first.insert_resource(UdpIdleTimeout(Duration::MAX));
    let a = idle_timeout_receiver(&mut first);
    first.update();
    let mut second = App::new();
    let b = idle_timeout_receiver(&mut second);
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
    let (_cr, mut cw, mut sr, _sw) = pair().await;
    let payload_limit = UdpOptions::DEFAULT.max_datagram_size - HEADER;
    send(&mut cw, vec![7; payload_limit + 1]).await;
    assert_eq!(cw.dropped_oversized_packets(), 1);
    send(&mut cw, vec![7; payload_limit]).await;
    assert_eq!(receive(&mut sr).await.unwrap(), vec![7; payload_limit]);
    send(&mut cw, vec![]).await;
    assert!(receive(&mut sr).await.unwrap().is_empty());
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
        send(write, vec![7; 3]).await;
        assert_eq!(write.dropped_oversized_packets(), 1);
        send(write, vec![8; 2]).await;
    }
    assert_eq!(receive(&mut cr).await.unwrap(), [8, 8]);
    assert_eq!(receive(&mut sr).await.unwrap(), [8, 8]);
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
        assert!(options.validate().is_err());
    }
}

#[test]
fn heartbeat_jitter_stays_inside_its_budget_and_varies() {
    let options = UdpOptions::DEFAULT;
    let mut seed = 42;
    let delays: Vec<_> = (0..100)
        .map(|_| heartbeat_delay(options, &mut seed))
        .collect();
    assert!(delays
        .iter()
        .all(|delay| *delay >= options.heartbeat_interval
            && *delay <= options.heartbeat_interval + options.heartbeat_jitter));
    assert!(delays.windows(2).any(|pair| pair[0] != pair[1]));
}

async fn raw_client(options: UdpOptions) -> (UdpSocket, UdpReadHalf, UdpWriteHalf) {
    let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let (client, ()) = tokio::join!(
        UdpClientStream::connect_with_options(server.local_addr().unwrap(), options),
        async {
            let mut buffer = [0; 256];
            loop {
                let (len, peer) = server.recv_from(&mut buffer).await.unwrap();
                let answer = test_answer(&buffer[..len]).unwrap();
                server.send_to(&answer, peer).await.unwrap();
                if buffer[4] == CONFIRM {
                    break;
                }
            }
        }
    );
    let (read, write) = client.unwrap().into_split().await.unwrap();
    (server, read, write)
}

#[tokio::test(start_paused = true)]
async fn application_traffic_suppresses_heartbeats_and_idle_resumes_them() {
    let (server, _read, mut write) = raw_client(UdpOptions::DEFAULT).await;
    let mut buffer = [0; 256];
    for _ in 0..8 {
        tokio::time::advance(Duration::from_millis(500)).await;
        send(&mut write, vec![7]).await;
        let len = server.recv(&mut buffer).await.unwrap();
        assert_eq!(parse_frame(&buffer[..len]).unwrap().0, DATA);
    }
    let last_data = Clock::now();
    let len = server.recv(&mut buffer).await.unwrap();
    assert_eq!(parse_frame(&buffer[..len]).unwrap().0, KEEPALIVE);
    assert!(last_data.elapsed() >= KEEPALIVE_INTERVAL);
    assert!(last_data.elapsed() <= (KEEPALIVE_INTERVAL + UdpOptions::DEFAULT.heartbeat_jitter) * 2);
}

#[tokio::test(start_paused = true)]
async fn idle_sessions_stay_alive_with_heartbeats() {
    let (mut cr, _cw, mut sr, mut sw) = pair().await;
    let server = tokio::spawn(async move { receive(&mut sr).await });
    let client = tokio::spawn(async move { receive(&mut cr).await });
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert!(!server.is_finished());
    assert!(!client.is_finished());
    send(&mut sw, vec![7]).await;
    assert_eq!(client.await.unwrap().unwrap(), [7]);
    server.abort();
}

#[tokio::test]
async fn client_ignores_stale_disconnect_and_malformed_frames() {
    let (server, mut read, write) = raw_client(UdpOptions::DEFAULT).await;
    let address = write.socket.local_addr().unwrap();
    for bytes in [
        control(DISCONNECT, &[0; 32]).to_vec(),
        vec![1],
        frame(KEEPALIVE, &write.state.id, &[1]),
        frame(DATA, &write.state.id, &[255]),
        frame(DATA, &write.state.id, &[7]),
    ] {
        server.send_to(&bytes, address).await.unwrap();
    }
    assert_eq!(receive(&mut read).await.unwrap(), [7]);
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
                .send_to(&control(DISCONNECT, &[0; 32]), peer)
                .await
                .unwrap();
            server
                .send_to(&test_answer(&buffer[..len]).unwrap(), peer)
                .await
                .unwrap();
            let (len, peer) = server.recv_from(&mut buffer).await.unwrap();
            let (tag, cookie) = Cookie::parse(&buffer[..len]).unwrap();
            assert_eq!(tag, CONFIRM);
            server
                .send_to(&control(DISCONNECT, &cookie.mac), peer)
                .await
                .unwrap();
        }
    );
    assert!(matches!(result, Err(err) if err.kind() == ErrorKind::ConnectionRefused));
}

#[tokio::test]
async fn replacement_registrations_also_consume_peer_slots() {
    let (listener, peer, _cookie, old) = fixture(UdpOptions {
        max_peers: 2,
        ..UdpOptions::DEFAULT
    })
    .await;
    let address = peer.local_addr().unwrap();
    let second = listener.cookies.issue(address, [2; 16]);
    let replacement = listener
        .dispatch(&second.encode(CONFIRM), address, Instant::now())
        .unwrap();
    let third = listener.cookies.issue(address, [3; 16]);
    assert!(listener
        .dispatch(&third.encode(CONFIRM), address, Instant::now())
        .is_none());
    drop(old);
    assert!(listener
        .dispatch(&third.encode(CONFIRM), address, Instant::now())
        .is_some());
    drop(replacement);
}

#[tokio::test]
async fn receiving_disconnect_closes_a_retained_read_half_and_writer() {
    let (server, mut read, mut write) = raw_client(UdpOptions::DEFAULT).await;
    server
        .send_to(
            &control(DISCONNECT, &write.state.id),
            write.socket.local_addr().unwrap(),
        )
        .await
        .unwrap();
    assert!(
        matches!(receive(&mut read).await, Err(ReceiveError::Io(err)) if err.kind() == ErrorKind::ConnectionAborted)
    );
    assert!(write
        .send::<Vec<u8>, _, _, _>(vec![7], Arc::new(Raw), &Ls::default())
        .await
        .is_err());
}
