use crate::client;
use crate::client::{ClientConnection, ClientPlugin, ConnectionEstablishEvent};
use crate::packet_length_serializer::LittleEndian;
use crate::protocols::tcp::TcpProtocol;
use crate::serializer::SerializerAdapter;
use crate::serializers::bitcode_serde::BitcodeSerdeSerializer;
use crate::server::{NewConnectionEvent, ServerAddress, ServerConnections, ServerPlugin};
use crate::{server, ClientConfig, ServerConfig};
use bevy::app::App;
use bevy::prelude::*;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Calls `step` until it returns `true` or a timeout expires.
pub(crate) fn wait_until(mut step: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !step() {
        assert!(
            Instant::now() < deadline,
            "condition was not met before timeout"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
struct Packet(u64);

macro_rules! test_config {
    ($name:ident, $protocol:ty) => {
        struct $name;

        impl ServerConfig for $name {
            type ClientPacket = Packet;
            type ServerPacket = Packet;
            type Protocol = $protocol;

            type EncodeError = bitcode::Error;
            type DecodeError = bitcode::Error;

            type LengthSerializer = LittleEndian<u32>;

            fn build_serializer() -> SerializerAdapter<
                Self::ClientPacket,
                Self::ServerPacket,
                Self::EncodeError,
                Self::DecodeError,
            > {
                SerializerAdapter::ReadOnly(Arc::new(BitcodeSerdeSerializer))
            }
        }

        impl ClientConfig for $name {
            type ClientPacket = Packet;
            type ServerPacket = Packet;
            type Protocol = $protocol;
            type EncodeError = bitcode::Error;
            type DecodeError = bitcode::Error;

            type LengthSerializer = LittleEndian<u32>;
            fn build_serializer() -> SerializerAdapter<
                Self::ServerPacket,
                Self::ClientPacket,
                Self::EncodeError,
                Self::DecodeError,
            > {
                SerializerAdapter::ReadOnly(Arc::new(BitcodeSerdeSerializer))
            }
        }
    };
}

test_config!(TcpConfig, TcpProtocol);
#[cfg(feature = "protocol_udp")]
test_config!(UdpConfig, crate::protocols::udp::UdpProtocol);

trait TestConfig:
    ServerConfig<ClientPacket = Packet, ServerPacket = Packet>
    + ClientConfig<ClientPacket = Packet, ServerPacket = Packet>
{
}

impl<C> TestConfig for C where
    C: ServerConfig<ClientPacket = Packet, ServerPacket = Packet>
        + ClientConfig<ClientPacket = Packet, ServerPacket = Packet>
{
}

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

#[derive(Default, Resource)]
struct ReceivedPackets<T> {
    packets: Vec<T>,
}

#[derive(Resource)]
struct ClientToServerPacketResource(Packet);

#[derive(Resource)]
struct ServerToClientPacketResource(Packet);

#[test]
fn tcp_packets() {
    let _apps = exchange_packets::<TcpConfig>();
}

#[cfg(feature = "protocol_udp")]
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

#[cfg(feature = "protocol_udp")]
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

fn exchange_packets<C: TestConfig>() -> (App, App) {
    let client_to_server_packet = Packet(42);
    let server_to_client_packet = Packet(24);

    let mut app_server = App::new();
    app_server.add_plugins(ServerPlugin::<C>::bind("127.0.0.1:0"));
    app_server.insert_resource(ReceivedPackets::<Packet>::default());
    app_server.insert_resource(ServerToClientPacketResource(server_to_client_packet));

    app_server.add_observer(server_new_connection_system::<C>);
    app_server.add_observer(server_packet_receive_system::<C>);

    app_server.update(); // bind
    let server_addr = app_server.world().resource::<ServerAddress<C>>().address();

    let mut app_client = App::new();
    app_client.add_plugins(ClientPlugin::<C>::connect(server_addr));
    app_client.insert_resource(ReceivedPackets::<Packet>::default());
    app_client.insert_resource(ClientToServerPacketResource(client_to_server_packet));

    app_client.add_observer(client_connection_establish_system::<C>);
    app_client.add_observer(client_packet_receive_system::<C>);

    wait_until(|| {
        app_client.update();
        app_server.update();
        !app_server
            .world()
            .resource::<ReceivedPackets<Packet>>()
            .packets
            .is_empty()
            && !app_client
                .world()
                .resource::<ReceivedPackets<Packet>>()
                .packets
                .is_empty()
    });

    // Check if the server received the packet from the client
    let server_received_packets = app_server
        .world()
        .get_resource::<ReceivedPackets<Packet>>()
        .unwrap();
    assert_eq!(
        server_received_packets.packets.first(),
        Some(&client_to_server_packet),
        "Server did not receive the expected packet from client"
    );

    // Check if the client received the packet from the server
    let client_received_packets = app_client
        .world()
        .get_resource::<ReceivedPackets<Packet>>()
        .unwrap();
    assert_eq!(
        client_received_packets.packets.first(),
        Some(&server_to_client_packet),
        "Client did not receive the expected packet from server"
    );
    (app_server, app_client)
}

fn server_new_connection_system<C: TestConfig>(
    event: On<NewConnectionEvent<C>>,
    server_to_client_packet: Res<ServerToClientPacketResource>,
) {
    event
        .event()
        .connection
        .send(server_to_client_packet.0)
        .expect("Couldn't send server packet");
}

fn server_packet_receive_system<C: TestConfig>(
    event: On<server::PacketReceiveEvent<C>>,
    mut received_packets: ResMut<ReceivedPackets<Packet>>,
) {
    assert!(event.event().received_at <= bevy::platform::time::Instant::now());
    received_packets.packets.push(event.event().packet);
}

fn client_connection_establish_system<C: TestConfig>(
    event: On<ConnectionEstablishEvent<C>>,
    client_to_server_packet: Res<ClientToServerPacketResource>,
) {
    event
        .event()
        .connection
        .send(client_to_server_packet.0)
        .expect("Couldn't send client packet");
}

fn client_packet_receive_system<C: TestConfig>(
    event: On<client::PacketReceiveEvent<C>>,
    mut received_packets: ResMut<ReceivedPackets<Packet>>,
) {
    assert!(event.event().received_at <= bevy::platform::time::Instant::now());
    received_packets.packets.push(event.event().packet);
}

#[cfg(feature = "protocol_udp")]
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
        let mut buffer = [0; 64];
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
            if len == 0 {
                socket.send_to(&[], source).unwrap();
            } else if buffer[..len] == [2] {
                socket.send_to(&[2], source).unwrap();
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

#[cfg(feature = "protocol_udp")]
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

#[cfg(feature = "protocol_udp")]
#[test]
fn retained_connection_transmits_after_disconnection_event() {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let address = socket.local_addr().unwrap();
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopped = Arc::clone(&stop);
    let (data_tx, data_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let mut buffer = [0; 64];
        while !stopped.load(std::sync::atomic::Ordering::Relaxed) {
            let Ok((len, source)) = socket.recv_from(&mut buffer) else {
                continue;
            };
            if len == 0 {
                socket.send_to(&[], source).unwrap();
            } else if buffer[0] == 1 {
                data_tx.send(buffer[..len].to_vec()).unwrap();
            } else if buffer[..len] == [2] {
                socket.send_to(&[2], source).unwrap();
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
