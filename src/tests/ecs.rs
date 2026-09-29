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
