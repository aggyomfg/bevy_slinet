use super::*;
use crate::connection::{MaxPacketSize, ReceiveLimits};
use crate::protocols::tcp::TcpProtocol;

test_config!(EcsTcpConfig, TcpProtocol);

#[test]
fn receive_limits_follow_each_apps_resource_insertion_update_and_removal() {
    let mut first = App::new();
    first.insert_resource(MaxPacketSize(16));
    first.add_plugins(ClientPlugin::<EcsTcpConfig>::new());
    first.update();

    let mut second = App::new();
    second.insert_resource(MaxPacketSize(32));
    second.add_plugins(ClientPlugin::<EcsTcpConfig>::new());
    second.update();

    let first_limits = first.world().resource::<ReceiveLimits>().clone();
    let second_limits = second.world().resource::<ReceiveLimits>().clone();
    assert_eq!(first_limits.max_packet_size(), 16);
    assert_eq!(second_limits.max_packet_size(), 32);

    first.insert_resource(MaxPacketSize(8));
    first.update();
    assert_eq!(first_limits.max_packet_size(), 8);
    assert_eq!(second_limits.max_packet_size(), 32);

    first.world_mut().remove_resource::<MaxPacketSize>();
    first.update();
    assert_eq!(first_limits.max_packet_size(), usize::MAX);
    assert_eq!(second_limits.max_packet_size(), 32);
}

#[derive(Default, Resource)]
struct ClientEventOrder(Vec<(&'static str, crate::connection::ConnectionId)>);

fn track_client_established(
    event: On<client::ConnectionEstablishEvent<EcsTcpConfig>>,
    mut order: ResMut<ClientEventOrder>,
) {
    order.0.push(("established", event.event().connection.id()));
}

fn track_client_packet(
    event: On<client::PacketReceiveEvent<EcsTcpConfig>>,
    mut order: ResMut<ClientEventOrder>,
) {
    let id = event.event().connection.id();
    assert!(order.0.contains(&("established", id)));
    order.0.push(("packet", id));
}

fn send_server_greeting(event: On<NewConnectionEvent<EcsTcpConfig>>) {
    event.event().connection.send(Packet(7)).unwrap();
}

#[test]
fn lifecycle_budget_publishes_each_connection_before_its_packets() {
    let queues = crate::connection::NetworkQueueSettings {
        events_per_frame: 1,
        ..Default::default()
    };
    let mut server = App::new();
    server.insert_resource(queues);
    server.add_plugins(ServerPlugin::<EcsTcpConfig>::bind("127.0.0.1:0"));
    server.add_observer(send_server_greeting);
    server.update();
    let address = server
        .world()
        .resource::<ServerAddress<EcsTcpConfig>>()
        .address();

    let mut client = App::new();
    client.insert_resource(queues);
    client.insert_resource(ClientEventOrder::default());
    client.add_plugins(ClientPlugin::<EcsTcpConfig>::new());
    client.add_observer(track_client_established);
    client.add_observer(track_client_packet);
    client.update();
    client
        .world_mut()
        .trigger(client::ConnectionRequestEvent::<EcsTcpConfig>::new(address));
    client
        .world_mut()
        .trigger(client::ConnectionRequestEvent::<EcsTcpConfig>::new(address));

    wait_until(|| {
        let before = server
            .world()
            .resource::<ServerConnections<EcsTcpConfig>>()
            .len();
        server.update();
        let after = server
            .world()
            .resource::<ServerConnections<EcsTcpConfig>>()
            .len();
        assert!(after <= before + 1);
        after == 2
    });

    client.update();
    assert_eq!(
        client
            .world()
            .resource::<client::ClientConnections<EcsTcpConfig>>()
            .len(),
        1
    );
    wait_until(|| {
        client.update();
        client
            .world()
            .resource::<ClientEventOrder>()
            .0
            .iter()
            .filter(|(kind, _)| *kind == "packet")
            .count()
            == 2
    });
    let events = &client.world().resource::<ClientEventOrder>().0;
    assert_eq!(
        events
            .iter()
            .filter(|(kind, _)| *kind == "established")
            .count(),
        2
    );
}

#[test]
fn endpoint_limits_are_isolated_and_restore_global_defaults() {
    use crate::client::ClientSettings;
    use crate::connection::settings::EndpointReceiveLimits;
    use crate::server::ServerSettings;
    test_config!(OtherConfig, TcpProtocol);
    let mut app = App::new();
    app.insert_resource(MaxPacketSize(64));
    app.insert_resource(ClientSettings::<EcsTcpConfig>::default().with_max_packet_size(8));
    app.insert_resource(ServerSettings::<EcsTcpConfig>::default().with_max_packet_size(16));
    app.add_plugins((
        ClientPlugin::<EcsTcpConfig>::new(),
        ClientPlugin::<OtherConfig>::new(),
        ServerPlugin::<EcsTcpConfig>::bind("127.0.0.1:0"),
    ));
    app.update();
    let client = app
        .world()
        .resource::<EndpointReceiveLimits<ClientPlugin<EcsTcpConfig>>>()
        .limits
        .clone();
    let server = app
        .world()
        .resource::<EndpointReceiveLimits<ServerPlugin<EcsTcpConfig>>>()
        .limits
        .clone();
    let other = app
        .world()
        .resource::<EndpointReceiveLimits<ClientPlugin<OtherConfig>>>()
        .limits
        .clone();
    assert_eq!(
        (
            client.max_packet_size(),
            server.max_packet_size(),
            other.max_packet_size()
        ),
        (8, 16, 64)
    );
    app.world_mut()
        .remove_resource::<ClientSettings<EcsTcpConfig>>();
    app.insert_resource(MaxPacketSize(128));
    app.update();
    assert_eq!(
        (
            client.max_packet_size(),
            server.max_packet_size(),
            other.max_packet_size()
        ),
        (128, 16, 128)
    );
}

#[test]
fn global_limits_system_is_registered_once_for_multiple_endpoints() {
    test_config!(OtherConfig, TcpProtocol);
    let mut app = App::new();
    app.add_plugins((
        ClientPlugin::<EcsTcpConfig>::new(),
        ClientPlugin::<OtherConfig>::new(),
        ServerPlugin::<EcsTcpConfig>::bind("127.0.0.1:0"),
    ));
    app.update();
    let schedules = app.world().resource::<Schedules>();
    for schedule in [
        schedules.get(Startup).unwrap(),
        schedules.get(Update).unwrap(),
    ] {
        let count = schedule
            .systems()
            .unwrap()
            .filter(|(_, system)| {
                system.system_type()
                    == IntoSystem::into_system(MaxPacketSize::set_system).system_type()
            })
            .count();
        assert_eq!(count, 1);
    }
}

#[test]
fn typed_settings_and_setup_sets_apply_commands_before_consumers() {
    use crate::client::{ClientSettings, ClientSystems};
    use crate::connection::settings::EndpointReceiveLimits;
    use crate::server::ServerSystems;
    let mut app = App::new();
    app.add_plugins((
        ClientPlugin::<EcsTcpConfig>::new(),
        ServerPlugin::<EcsTcpConfig>::bind("127.0.0.1:0"),
    ));
    app.add_systems(
        Startup,
        (
            (|mut commands: Commands| {
                commands.insert_resource(
                    ClientSettings::<EcsTcpConfig>::default().with_max_packet_size(17),
                );
            })
            .before(ClientSystems::<EcsTcpConfig>::SETTINGS),
            (|limits: Res<EndpointReceiveLimits<ClientPlugin<EcsTcpConfig>>>| {
                assert_eq!(limits.limits.max_packet_size(), 17);
            })
            .after(ClientSystems::<EcsTcpConfig>::SETUP),
            (|address: Res<ServerAddress<EcsTcpConfig>>| {
                assert_ne!(address.address().port(), 0);
            })
            .after(ServerSystems::<EcsTcpConfig>::SETUP),
        ),
    );
    app.add_systems(
        Update,
        (
            (|mut commands: Commands| {
                commands.insert_resource(
                    ClientSettings::<EcsTcpConfig>::default().with_max_packet_size(23),
                );
            })
            .before(ClientSystems::<EcsTcpConfig>::SETTINGS),
            (|limits: Res<EndpointReceiveLimits<ClientPlugin<EcsTcpConfig>>>| {
                assert_eq!(limits.limits.max_packet_size(), 23);
            })
            .after(ClientSystems::<EcsTcpConfig>::SETTINGS),
        ),
    );
    app.update();
}

#[cfg(feature = "protocol_udp")]
#[test]
fn server_packet_ordering_does_not_depend_on_other_config_lifecycle() {
    use crate::server::ServerSystems;
    test_config!(UdpConfig, crate::protocols::udp::UdpProtocol);
    let mut app = App::new();
    app.add_plugins((
        ServerPlugin::<UdpConfig>::bind("127.0.0.1:0"),
        ServerPlugin::<EcsTcpConfig>::bind("127.0.0.1:0"),
    ));
    // With a global lifecycle dependency this creates a cycle. Independent
    // endpoints must allow either relative ordering chosen by the application.
    app.configure_sets(
        PreUpdate,
        ServerSystems::<EcsTcpConfig>::LIFECYCLE.after(ServerSystems::<UdpConfig>::PACKETS),
    );
    app.update();
}

#[test]
fn rejected_runtime_keeps_endpoint_systems_valid() {
    use crate::runtime::{NetworkRuntime, NetworkRuntimeMode, NetworkRuntimeSettings};

    let external = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut app = App::new();
    app.insert_resource(NetworkRuntimeSettings {
        mode: NetworkRuntimeMode::External(external.handle().clone()),
        ..Default::default()
    });
    app.add_plugins((
        ClientPlugin::<EcsTcpConfig>::new(),
        ServerPlugin::<EcsTcpConfig>::bind("127.0.0.1:0"),
    ));
    app.update();
    app.update();
    assert!(!app.world().resource::<NetworkRuntime>().is_available());
    assert!(!app
        .world()
        .contains_resource::<ServerAddress<EcsTcpConfig>>());
}

fn external_runtime_app(handle: &tokio::runtime::Handle) -> App {
    use crate::runtime::{NetworkRuntimeMode, NetworkRuntimeSettings};

    let mut app = App::new();
    app.insert_resource(NetworkRuntimeSettings {
        mode: NetworkRuntimeMode::External(handle.clone()),
        ..Default::default()
    });
    app.init_resource::<ReceivedPackets<Packet>>();
    app
}

fn external_runtime_tcp_server(handle: &tokio::runtime::Handle) -> (App, std::net::SocketAddr) {
    let mut app = external_runtime_app(handle);
    app.add_plugins(ServerPlugin::<EcsTcpConfig>::bind("127.0.0.1:0"));
    app.add_observer(server_packet_receive_system::<EcsTcpConfig>);
    app.update();
    let address = app
        .world()
        .resource::<ServerAddress<EcsTcpConfig>>()
        .address();
    (app, address)
}

#[test]
fn dropping_one_app_releases_its_connections_and_preserves_shared_runtime_peers() {
    use crate::runtime::NetworkRuntime;

    let external = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (mut first, first_address) = external_runtime_tcp_server(external.handle());
    let (mut second, second_address) = external_runtime_tcp_server(external.handle());
    let mut client = external_runtime_app(external.handle());
    client.add_plugins(ClientPlugin::<EcsTcpConfig>::connect(first_address));
    client.add_observer(client_packet_receive_system::<EcsTcpConfig>);
    client.update();
    client
        .world_mut()
        .trigger(client::ConnectionRequestEvent::<EcsTcpConfig>::new(
            second_address,
        ));
    wait_until(|| {
        first.update();
        second.update();
        client.update();
        first
            .world()
            .resource::<ServerConnections<EcsTcpConfig>>()
            .len()
            == 1
            && second
                .world()
                .resource::<ServerConnections<EcsTcpConfig>>()
                .len()
                == 1
            && client
                .world()
                .resource::<client::ClientConnections<EcsTcpConfig>>()
                .len()
                == 2
    });
    let first_connection = first
        .world()
        .resource::<ServerConnections<EcsTcpConfig>>()
        .first()
        .unwrap()
        .clone();
    let second_connection = second
        .world()
        .resource::<ServerConnections<EcsTcpConfig>>()
        .first()
        .unwrap()
        .clone();
    let client_connection = client
        .world()
        .resource::<client::ClientConnections<EcsTcpConfig>>()
        .iter()
        .find(|connection| connection.peer_addr() == second_address)
        .unwrap()
        .clone();

    drop(first);
    assert!(first_connection.is_closed());
    assert!(matches!(
        first_connection.send(Packet(1)),
        Err(crate::connection::SendError::Closed(Packet(1)))
    ));
    let rebound = external
        .block_on(tokio::net::TcpListener::bind(first_address))
        .unwrap();
    assert_eq!(rebound.local_addr().unwrap(), first_address);
    assert!(second.world().resource::<NetworkRuntime>().is_available());
    assert!(!second_connection.is_closed());
    second_connection.send(Packet(24)).unwrap();
    client_connection.send(Packet(42)).unwrap();
    wait_until(|| {
        second.update();
        client.update();
        second.world().resource::<ReceivedPackets<Packet>>().packets == [Packet(42)]
            && client.world().resource::<ReceivedPackets<Packet>>().packets == [Packet(24)]
    });
}
