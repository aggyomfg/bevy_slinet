use super::*;

use crate::protocols::udp::UdpProtocol;

test_config!(UdpConfig, UdpProtocol);

#[test]
fn udp_packets_and_disconnection() {
    let (mut app_server, mut app_client) = exchange_packets::<UdpConfig>();

    app_server
        .world()
        .resource::<ServerConnections<UdpConfig>>()[0]
        .disconnect();
    wait_until(|| {
        app_client.update();
        app_server.update();
        app_server
            .world()
            .resource::<ServerConnections<UdpConfig>>()
            .is_empty()
    });
    assert!(app_server
        .world()
        .resource::<ServerConnections<UdpConfig>>()
        .is_empty());

    // The server's disconnect datagrams close the client too.
    wait_until(|| {
        app_client.update();
        app_server.update();
        !app_client
            .world()
            .contains_resource::<ClientConnection<UdpConfig>>()
    });
    assert!(!app_client
        .world()
        .contains_resource::<ClientConnection<UdpConfig>>());
}

#[test]
fn silent_udp_endpoint_does_not_block_other_connections() {
    // Keep the socket open without answering, so the first handshake waits for its timeout.
    let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut server = App::new();
    server.add_plugins(ServerPlugin::<UdpConfig>::bind("127.0.0.1:0"));
    server.update();
    let address = server
        .world()
        .resource::<ServerAddress<UdpConfig>>()
        .address();
    let mut client = App::new();
    client.add_plugins(ClientPlugin::<UdpConfig>::new());
    client.update();
    client
        .world_mut()
        .trigger(client::ConnectionRequestEvent::<UdpConfig>::new(
            silent.local_addr().unwrap(),
        ));
    client
        .world_mut()
        .trigger(client::ConnectionRequestEvent::<UdpConfig>::new(address));
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        client.update();
        server.update();
        if let Some(connection) = client.world().get_resource::<ClientConnection<UdpConfig>>() {
            assert_eq!(connection.peer_addr(), address);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "live endpoint was blocked by a silent handshake"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn failed_parallel_attempt_must_not_remove_live_connection() {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let worker = std::thread::spawn(move || {
        let mut ignored = None;
        let mut buffer = [0; 256];
        while !stopped.load(std::sync::atomic::Ordering::Relaxed) {
            let Ok((len, source)) = socket.recv_from(&mut buffer) else {
                continue;
            };
            if ignored.is_none() {
                ignored = Some(source);
            }
            if ignored == Some(source) {
                continue;
            }
            if let Some(response) =
                crate::protocols::udp::test_support::RawPeer::respond(&buffer[..len])
            {
                socket.send_to(&response, source).unwrap();
            }
        }
    });
    let mut client = App::new();
    client.add_plugins(ClientPlugin::<UdpConfig>::new());
    client.update();
    for _ in 0..2 {
        client
            .world_mut()
            .trigger(client::ConnectionRequestEvent::<UdpConfig>::new(address));
    }
    wait_until(|| {
        client.update();
        client
            .world()
            .resource::<client::ClientConnections<UdpConfig>>()
            .len()
            == 1
    });
    assert_eq!(
        client
            .world()
            .resource::<client::ClientConnections<UdpConfig>>()
            .len(),
        1
    );
    let established = Instant::now();
    while established.elapsed() < Duration::from_secs(6) {
        client.update();
        std::thread::sleep(Duration::from_millis(10));
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    worker.join().unwrap();
    assert_eq!(
        client
            .world()
            .resource::<client::ClientConnections<UdpConfig>>()
            .len(),
        1,
        "the live connection must survive failure of another attempt to the same peer_addr"
    );
}

#[test]
fn retained_connection_must_reject_send_after_disconnect() {
    let (mut server, mut client) = exchange_packets::<UdpConfig>();
    let retained = client
        .world()
        .resource::<ClientConnection<UdpConfig>>()
        .clone();
    retained.disconnect();
    wait_until(|| {
        client.update();
        server.update();
        client
            .world()
            .resource::<client::ClientConnections<UdpConfig>>()
            .is_empty()
    });
    assert!(client
        .world()
        .resource::<client::ClientConnections<UdpConfig>>()
        .is_empty());
    assert!(
        retained.send(Packet(99)).is_err(),
        "a retained connection still accepts packets after the disconnection event"
    );
}

#[test]
fn retained_connection_cannot_transmit_after_disconnection_event() {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let (data_tx, data_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut buffer = [0; 256];
        while !stopped.load(std::sync::atomic::Ordering::Relaxed) {
            let Ok((len, source)) = socket.recv_from(&mut buffer) else {
                continue;
            };
            if let Some(response) =
                crate::protocols::udp::test_support::RawPeer::respond(&buffer[..len])
            {
                socket.send_to(&response, source).unwrap();
            } else if buffer[..len].starts_with(b"SLN2\x01") {
                data_tx.send(buffer[..len].to_vec()).unwrap();
            }
        }
    });
    let disconnected = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let event_observed = Arc::clone(&disconnected);
    let mut client = App::new();
    client.add_plugins(ClientPlugin::<UdpConfig>::connect(address));
    client.add_observer(move |_: On<client::DisconnectionEvent<UdpConfig>>| {
        event_observed.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    wait_until(|| {
        client.update();
        client
            .world()
            .contains_resource::<ClientConnection<UdpConfig>>()
    });
    let retained = client
        .world()
        .resource::<ClientConnection<UdpConfig>>()
        .clone();
    retained.disconnect();
    wait_until(|| {
        client.update();
        disconnected.load(std::sync::atomic::Ordering::Relaxed)
    });
    assert!(disconnected.load(std::sync::atomic::Ordering::Relaxed));
    let send_result = retained.send(Packet(99));
    let wire_result = data_rx.recv_timeout(Duration::from_secs(1));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    worker.join().unwrap();
    assert!(wire_result.is_err(), "after DisconnectionEvent retained.send returned {send_result:?} and peer received {wire_result:?}");
}

#[test]
fn disconnect_removes_only_one_connection_to_the_same_address() {
    let mut server = App::new();
    server.add_plugins(ServerPlugin::<UdpConfig>::bind("127.0.0.1:0"));
    server.update();
    let address = server
        .world()
        .resource::<ServerAddress<UdpConfig>>()
        .address();
    let mut client = App::new();
    client.add_plugins(ClientPlugin::<UdpConfig>::new());
    client.update();
    for _ in 0..2 {
        client
            .world_mut()
            .trigger(client::ConnectionRequestEvent::<UdpConfig>::new(address));
    }
    wait_until(|| {
        client.update();
        server.update();
        client
            .world()
            .resource::<client::ClientConnections<UdpConfig>>()
            .len()
            == 2
            && server
                .world()
                .resource::<ServerConnections<UdpConfig>>()
                .len()
                == 2
    });
    let connections = client
        .world()
        .resource::<client::ClientConnections<UdpConfig>>();
    let survivor = connections[1].id();
    connections[0].disconnect();
    wait_until(|| {
        client.update();
        server.update();
        client
            .world()
            .resource::<client::ClientConnections<UdpConfig>>()
            .len()
            == 1
    });
    assert_eq!(
        client
            .world()
            .resource::<client::ClientConnections<UdpConfig>>()[0]
            .id(),
        survivor
    );
}

#[test]
fn retained_server_connection_rejects_sends_after_remote_disconnect() {
    let (mut server, mut client) = exchange_packets::<UdpConfig>();
    let retained = server.world().resource::<ServerConnections<UdpConfig>>()[0].clone();
    client
        .world()
        .resource::<ClientConnection<UdpConfig>>()
        .disconnect();
    wait_until(|| {
        server.update();
        client.update();
        server
            .world()
            .resource::<ServerConnections<UdpConfig>>()
            .is_empty()
    });
    assert!(retained.send(Packet(99)).is_err());
}
