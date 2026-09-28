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
