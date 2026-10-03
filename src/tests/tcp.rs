use super::*;

use crate::protocols::tcp::TcpProtocol;
use std::net::SocketAddr;

test_config!(TcpConfig, TcpProtocol);

#[derive(Default, Resource)]
struct NewConnectionAddress(Option<SocketAddr>);

#[test]
fn tcp_connection() {
    let mut app_server = App::new();
    app_server.add_plugins(ServerPlugin::<TcpConfig>::bind("127.0.0.1:0"));
    app_server.init_resource::<NewConnectionAddress>();
    app_server.add_observer(
        |event: On<NewConnectionEvent<TcpConfig>>, mut address: ResMut<NewConnectionAddress>| {
            address.0 = Some(event.event().address);
        },
    );
    app_server.update(); // bind
    let server_addr = app_server
        .world()
        .resource::<ServerAddress<TcpConfig>>()
        .address();

    let mut app_client = App::new();
    app_client.add_plugins(ClientPlugin::<TcpConfig>::connect(server_addr));

    wait_until(|| {
        app_client.update();
        app_server.update();
        app_client
            .world()
            .contains_resource::<ClientConnection<TcpConfig>>()
            && app_server
                .world()
                .resource::<ServerConnections<TcpConfig>>()
                .len()
                == 1
    });

    assert!(
        app_client
            .world()
            .get_resource::<ClientConnection<TcpConfig>>()
            .is_some(),
        "No ClientConnection resource found"
    );
    assert_eq!(
        app_server
            .world()
            .get_resource::<ServerConnections<TcpConfig>>()
            .unwrap()
            .len(),
        1,
    );
    assert_eq!(
        app_server.world().resource::<NewConnectionAddress>().0,
        Some(
            app_client
                .world()
                .resource::<ClientConnection<TcpConfig>>()
                .local_addr()
        ),
        "NewConnectionEvent.address must be the client's address"
    );
}

#[test]
fn tcp_packets() {
    let _apps = exchange_packets::<TcpConfig>();
}

#[test]
fn ecs_packet_budget_limits_each_frame() {
    let (mut server, mut client) = exchange_packets::<TcpConfig>();
    server.insert_resource(crate::connection::NetworkQueueSettings {
        events_per_frame: 1,
        ..Default::default()
    });
    let connection = client
        .world()
        .resource::<ClientConnection<TcpConfig>>()
        .clone();
    for n in 0..16 {
        connection.send(Packet(n)).unwrap();
    }
    wait_until(|| {
        let before = server
            .world()
            .resource::<ReceivedPackets<Packet>>()
            .packets
            .len();
        server.update();
        client.update();
        let after = server
            .world()
            .resource::<ReceivedPackets<Packet>>()
            .packets
            .len();
        assert!(after <= before + 1);
        after == 17
    });
}

#[test]
fn tcp_disconnect_is_reported_while_ecs_queue_is_full() {
    let queues = crate::connection::NetworkQueueSettings {
        receive_capacity: 1,
        ..Default::default()
    };
    let mut server = App::new();
    server.insert_resource(queues);
    server.add_plugins(ServerPlugin::<TcpConfig>::bind("127.0.0.1:0"));
    server.update();
    let address = server
        .world()
        .resource::<ServerAddress<TcpConfig>>()
        .address();
    let mut client = App::new();
    client.insert_resource(queues);
    client.add_plugins(ClientPlugin::<TcpConfig>::connect(address));
    wait_until(|| {
        server.update();
        client.update();
        !server
            .world()
            .resource::<ServerConnections<TcpConfig>>()
            .is_empty()
            && client
                .world()
                .contains_resource::<ClientConnection<TcpConfig>>()
    });
    let server_connection = server.world().resource::<ServerConnections<TcpConfig>>()[0].clone();
    let client_connection = client
        .world()
        .resource::<ClientConnection<TcpConfig>>()
        .clone();
    for n in 0..16 {
        client_connection.send(Packet(n)).unwrap();
        server_connection.send(Packet(n)).unwrap();
    }
    // Stop draining ECS while network tasks fill the one-packet queues.
    std::thread::sleep(Duration::from_millis(100));
    client_connection.disconnect();
    server_connection.disconnect();
    wait_until(|| {
        server.update();
        client.update();
        server
            .world()
            .resource::<ServerConnections<TcpConfig>>()
            .is_empty()
            && client
                .world()
                .resource::<client::ClientConnections<TcpConfig>>()
                .is_empty()
    });
}

#[test]
fn encode_error_preserves_disconnection_cause() {
    struct BadCodec;
    impl crate::serializer::ReadOnlySerializer<Packet, Packet> for BadCodec {
        type EncodeError = std::io::Error;
        type DecodeError = std::io::Error;
        fn serialize(&self, _: Packet) -> std::io::Result<Vec<u8>> {
            Err(std::io::Error::other("review encoding failure"))
        }
        fn deserialize(&self, _: &[u8]) -> std::io::Result<Packet> {
            Ok(Packet(0))
        }
    }
    struct BadConfig;
    impl ClientConfig for BadConfig {
        type ClientPacket = Packet;
        type ServerPacket = Packet;
        type Protocol = TcpProtocol;
        type EncodeError = std::io::Error;
        type DecodeError = std::io::Error;
        type LengthSerializer = LittleEndian<u32>;
        fn build_serializer() -> SerializerAdapter<Packet, Packet, std::io::Error, std::io::Error> {
            SerializerAdapter::ReadOnly(Arc::new(BadCodec))
        }
    }
    #[derive(Default, Resource)]
    struct Outcome(Option<bool>);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut client = App::new();
    client.init_resource::<Outcome>();
    client.add_plugins(ClientPlugin::<BadConfig>::connect(
        listener.local_addr().unwrap(),
    ));
    client.add_observer(|ev: On<ConnectionEstablishEvent<BadConfig>>| {
        ev.connection.send(Packet(1)).unwrap();
    });
    client.add_observer(|ev: On<client::DisconnectionEvent<BadConfig>>, mut result: ResMut<Outcome>| {
        result.0 = Some(matches!(&ev.error, crate::protocol::ReceiveError::Io(error) if error.to_string().contains("review encoding failure")));
    });
    client.update();
    let (_socket, _) = listener.accept().unwrap();
    wait_until(|| {
        client.update();
        client.world().resource::<Outcome>().0.is_some()
    });
    assert_eq!(client.world().resource::<Outcome>().0, Some(true));
}

#[derive(Default, Resource)]
struct EventOrder(Vec<&'static str>);

#[test]
fn tcp_eof_follows_all_buffered_packets() {
    use crate::serializer::ReadOnlySerializer;
    use std::io::Write;
    let mut server = App::new();
    server.insert_resource(crate::connection::NetworkQueueSettings {
        events_per_frame: 1,
        ..Default::default()
    });
    server.init_resource::<EventOrder>();
    server.add_plugins(ServerPlugin::<TcpConfig>::bind("127.0.0.1:0"));
    server.add_observer(
        |_: On<NewConnectionEvent<TcpConfig>>, mut order: ResMut<EventOrder>| order.0.push("open"),
    );
    server.add_observer(
        |_: On<server::PacketReceiveEvent<TcpConfig>>, mut order: ResMut<EventOrder>| {
            order.0.push("packet");
        },
    );
    server.add_observer(
        |_: On<server::DisconnectionEvent<TcpConfig>>, mut order: ResMut<EventOrder>| {
            order.0.push("close");
        },
    );
    server.update();
    let address = server
        .world()
        .resource::<ServerAddress<TcpConfig>>()
        .address();
    let mut socket = std::net::TcpStream::connect(address).unwrap();
    for n in 0..3 {
        let bytes =
            ReadOnlySerializer::<Packet, Packet>::serialize(&BitcodeSerdeSerializer, Packet(n))
                .unwrap();
        socket
            .write_all(&u32::try_from(bytes.len()).unwrap().to_le_bytes())
            .unwrap();
        socket.write_all(&bytes).unwrap();
    }
    socket.shutdown(std::net::Shutdown::Write).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    wait_until(|| {
        server.update();
        server.world().resource::<EventOrder>().0.len() == 5
    });
    assert_eq!(
        server.world().resource::<EventOrder>().0,
        ["open", "packet", "packet", "packet", "close"]
    );
}

#[test]
fn pending_accepts_are_bounded_while_ecs_is_paused() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static BUILT: AtomicUsize = AtomicUsize::new(0);
    struct Counted;
    impl ServerConfig for Counted {
        type ClientPacket = Packet;
        type ServerPacket = Packet;
        type Protocol = TcpProtocol;
        type EncodeError = bitcode::Error;
        type DecodeError = bitcode::Error;
        type LengthSerializer = LittleEndian<u32>;
        fn build_serializer() -> SerializerAdapter<Packet, Packet, bitcode::Error, bitcode::Error> {
            BUILT.fetch_add(1, Ordering::SeqCst);
            SerializerAdapter::ReadOnly(Arc::new(BitcodeSerdeSerializer))
        }
    }
    let mut app = App::new();
    app.insert_resource(crate::connection::NetworkQueueSettings {
        receive_capacity: 1,
        events_per_frame: 1,
        ..Default::default()
    });
    app.add_plugins(ServerPlugin::<Counted>::bind("127.0.0.1:0"));
    app.update();
    let addr = app.world().resource::<ServerAddress<Counted>>().address();
    let sockets: Vec<_> = (0..32)
        .map(|_| std::net::TcpStream::connect(addr).unwrap())
        .collect();
    wait_until(|| BUILT.load(Ordering::SeqCst) >= 1);
    std::thread::sleep(Duration::from_millis(100));
    assert!(BUILT.load(Ordering::SeqCst) <= 2);
    drop(sockets);
}
