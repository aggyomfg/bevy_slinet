//! Socket-free fixtures invoking the production ECS systems.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::cast_precision_loss
)]
#[allow(clippy::wildcard_imports)]
use super::*;
use crate::bench_utils::{Delivery, Scenario};
use crate::protocols::tcp::TcpProtocol;
use crate::protocols::udp::{UdpConnectionHandle, UdpProtocol};
use crate::serializers::{
    bitcode::BitcodeSerializer, packet_length_serializer::LittleEndian,
    serializer::SerializerAdapter,
};
use std::convert::Infallible;
use std::sync::atomic::AtomicBool;
use tokio_util::sync::CancellationToken;

struct Config<P = TcpProtocol>(PhantomData<P>);
impl<P: Protocol> ClientConfig for Config<P> {
    type ClientPacket = u64;
    type ServerPacket = u64;
    type Protocol = P;
    type EncodeError = Infallible;
    type DecodeError = bitcode::Error;
    type LengthSerializer = LittleEndian<u32>;
    fn build_serializer() -> SerializerAdapter<u64, u64, Infallible, bitcode::Error> {
        SerializerAdapter::ReadOnly(Arc::new(BitcodeSerializer))
    }
}
#[derive(Resource, Default)]
struct Counts(Delivery);
#[derive(Resource, Default)]
struct UpdatePackets(usize);

pub struct Fixture<P: Protocol = TcpProtocol> {
    app: App,
    sender: Sender<ClientLifecycle<Config<P>>>,
    connections: Vec<ClientConnection<Config<P>>>,
    _outgoing: Vec<OutgoingReceiver<u64, P::Handle>>,
    packets: LossySender<PacketReceiveEvent<Config<P>>>,
    scenario: Scenario,
    packets_per_connection: usize,
}
impl Fixture {
    #[must_use]
    pub fn new(
        budget: usize,
        peers: usize,
        packets_per_connection: usize,
        scenario: Scenario,
    ) -> Self {
        Self::with_transport(budget, peers, packets_per_connection, scenario, || ())
    }
}
impl Fixture<UdpProtocol> {
    #[must_use]
    pub fn udp(budget: usize, peers: usize, packets_per_connection: usize) -> Self {
        Self::with_transport(
            budget,
            peers,
            packets_per_connection,
            Scenario::Packets,
            || UdpConnectionHandle::new(1200, None),
        )
    }
}
impl<P: Protocol> Fixture<P> {
    fn with_transport(
        budget: usize,
        peers: usize,
        packets_per_connection: usize,
        scenario: Scenario,
        transport: impl Fn() -> P::Handle,
    ) -> Self {
        assert!(budget > 0 && peers > 0 && packets_per_connection > 0);
        let settings = NetworkQueueSettings {
            receive_capacity: peers * (packets_per_connection + 2),
            events_per_frame: budget,
            ..Default::default()
        };
        let (sender, receiver) = settings.incoming_channel();
        let (packet_sender, packets) = lossy_channel(
            if P::DATAGRAM {
                peers * packets_per_connection
            } else {
                1
            },
            usize::MAX,
            crate::connection::OverflowPolicy::DropNewest,
        );
        let mut app = App::new();
        app.insert_resource(settings)
            .insert_resource(LifecycleReceiver::<Config<P>>(receiver))
            .insert_resource(PacketReceiver::<Config<P>> { receiver: packets })
            .insert_resource(ClientConnections::<Config<P>>::new())
            .init_resource::<Counts>()
            .add_observer(
                |_: On<ConnectionEstablishEvent<Config<P>>>, mut counts: ResMut<Counts>| {
                    counts.0.established += 1;
                },
            )
            .add_observer(
                |_: On<PacketReceiveEvent<Config<P>>>, mut counts: ResMut<Counts>| {
                    counts.0.packets += 1;
                    counts.0.last_packet_frame = counts.0.frames;
                },
            )
            .add_observer(
                |_: On<DisconnectionEvent<Config<P>>>, mut counts: ResMut<Counts>| {
                    counts.0.closed += 1;
                    counts.0.last_close_frame = counts.0.frames;
                },
            );
        configure_receive_systems::<Config<P>>(&mut app);
        if P::DATAGRAM {
            app.init_resource::<UpdatePackets>().add_systems(
                Update,
                |counts: Res<Counts>, mut visible: ResMut<UpdatePackets>| {
                    visible.0 = counts.0.packets;
                },
            );
        }
        app.update(); // Initialize schedules outside measurements.
        let (connections, outgoing) = (0..peers)
            .map(|_| {
                let (packet_tx, rx) = settings.outgoing_channel(P::DATAGRAM);
                let connection = EcsConnection {
                    disconnect_task: CancellationToken::new(),
                    id: ConnectionId::next(),
                    published: Arc::new(AtomicBool::new(false)),
                    packet_tx,
                    transport: transport(),
                    local_addr: ([127, 0, 0, 1], 1000).into(),
                    peer_addr: ([127, 0, 0, 1], 2000).into(),
                };
                (connection, rx)
            })
            .unzip();
        let mut result = Self {
            app,
            sender,
            packets: packet_sender,
            connections,
            _outgoing: outgoing,
            scenario,
            packets_per_connection,
        };
        if matches!(scenario, Scenario::Packets) {
            for connection in &result.connections {
                result.establish(connection);
            }
            while result.app.world().resource::<Counts>().0.established < peers {
                result.app.update();
            }
        }
        result
    }
    fn establish(&self, connection: &ClientConnection<Config<P>>) {
        assert!(self
            .sender
            .try_send(ClientLifecycle::Established(ConnectionEstablishEvent {
                address: connection.peer_addr(),
                connection: connection.clone()
            }))
            .is_ok());
    }
    /// Enqueue outside the timing/allocation region; all events fit without blocking.
    pub fn enqueue(&mut self) {
        self.app.world_mut().resource_mut::<Counts>().0 = Delivery::default();
        if P::DATAGRAM {
            self.app.world_mut().resource_mut::<UpdatePackets>().0 = 0;
        }
        for connection in &self.connections {
            if matches!(self.scenario, Scenario::Interleaved) {
                self.establish(connection);
            }
            for packet in 0..self.packets_per_connection {
                let event = PacketReceiveEvent {
                    connection: connection.clone(),
                    packet: packet as u64,
                    received_at: Instant::now(),
                };
                if P::DATAGRAM {
                    assert!(self.packets.try_send(event, size_of::<u64>()).is_ok());
                } else {
                    assert!(self.sender.try_send(ClientLifecycle::Packet(event)).is_ok());
                }
            }
            if matches!(self.scenario, Scenario::Interleaved) {
                let event = ConnectionClosed::new(
                    ReceiveError::IntentionalDisconnection,
                    connection.peer_addr(),
                    Some(connection.id()),
                );
                assert!(self.sender.try_send(ClientLifecycle::Closed(event)).is_ok());
            }
        }
    }
    pub fn drain(&mut self) -> Delivery {
        let expected_packets = self.connections.len() * self.packets_per_connection;
        let expected_closed = if matches!(self.scenario, Scenario::Interleaved) {
            self.connections.len()
        } else {
            0
        };
        for _ in 0..(expected_packets + self.connections.len() * 2 + 2) {
            self.app.world_mut().resource_mut::<Counts>().0.frames += 1;
            self.app.update();
            let result = self.app.world().resource::<Counts>().0;
            // UDP measures gameplay visibility, not just observer publication.
            let visible = if P::DATAGRAM {
                self.app.world().resource::<UpdatePackets>().0
            } else {
                result.packets
            };
            if visible == expected_packets && result.closed == expected_closed {
                return result;
            }
        }
        panic!("ECS fixture failed to drain its finite event sequence");
    }
}

#[cfg(test)]
mod tests {
    #[allow(clippy::wildcard_imports)]
    use super::*;
    #[test]
    fn udp_fixture_measures_delivery_to_update_and_respects_budget() {
        for (budget, frames) in [(16, 8), (256, 1)] {
            let mut fixture = Fixture::<crate::protocols::udp::UdpProtocol>::udp(budget, 32, 4);
            for _ in 0..2 {
                fixture.enqueue();
                let result = fixture.drain();
                assert_eq!(result.packets, 128);
                assert_eq!(result.frames, frames);
                assert_eq!(result.last_packet_frame, frames);
            }
        }
    }

    #[test]
    fn mixed_fifo_drains_without_phase_delay_and_respects_total_budget() {
        for (scenario, budget, frames) in [
            (Scenario::Packets, 1, 8),
            (Scenario::Packets, 256, 1),
            (Scenario::Interleaved, 256, 1),
            (Scenario::Interleaved, 1, 24),
            (Scenario::Interleaved, 5, 5),
        ] {
            let mut fixture = Fixture::new(budget, 8, 1, scenario);
            fixture.enqueue();
            let result = fixture.drain();
            assert_eq!(result.packets, 8);
            assert_eq!(result.frames, frames);
            if matches!(scenario, Scenario::Interleaved) {
                assert_eq!(result.established, 8);
                assert_eq!(result.closed, 8);
                assert_eq!(result.last_packet_frame, 23_usize.div_ceil(budget));
                assert_eq!(result.last_close_frame, 24_usize.div_ceil(budget));
            }
            fixture.enqueue();
            assert_eq!(fixture.drain(), result);
        }
    }

    #[test]
    fn observers_see_registry_changes_in_fifo_order_in_one_frame() {
        #[derive(Resource, Default)]
        struct Order(Vec<&'static str>);
        let mut fixture = Fixture::new(256, 2, 1, Scenario::Interleaved);
        fixture.app.init_resource::<Order>();
        fixture.app.add_observer(
            |event: On<ConnectionEstablishEvent<Config>>,
             connections: Res<ClientConnections<Config>>,
             mut order: ResMut<Order>| {
                assert_eq!(connections.len(), 1);
                assert_eq!(connections.first().unwrap().id(), event.connection.id());
                order.0.push("open");
            },
        );
        fixture.app.add_observer(
            |event: On<PacketReceiveEvent<Config>>,
             connections: Res<ClientConnections<Config>>,
             mut order: ResMut<Order>| {
                assert_eq!(connections.len(), 1);
                assert_eq!(connections.first().unwrap().id(), event.connection.id());
                order.0.push("packet");
            },
        );
        fixture.app.add_observer(
            |event: On<DisconnectionEvent<Config>>,
             connections: Res<ClientConnections<Config>>,
             mut order: ResMut<Order>| {
                assert!(connections
                    .iter()
                    .all(|connection| connection.id() != event.connection_id.unwrap()));
                order.0.push("close");
            },
        );
        fixture.enqueue();
        fixture.app.update();
        assert_eq!(
            fixture.app.world().resource::<Order>().0,
            ["open", "packet", "close", "open", "packet", "close"]
        );
    }
    #[test]
    fn receive_sets_expose_observer_effects_in_pre_update() {
        let mut fixture = Fixture::new(256, 1, 1, Scenario::Interleaved);
        fixture.app.add_systems(
            PreUpdate,
            (
                (|counts: Res<Counts>| assert_eq!(counts.0.packets, 0))
                    .before(ClientSystems::<Config>::RECEIVE),
                (|counts: Res<Counts>| {
                    assert_eq!(counts.0.established, 1);
                    assert_eq!(counts.0.packets, 1);
                    assert_eq!(counts.0.closed, 1);
                })
                .after(ClientSystems::<Config>::RECEIVE),
                (|counts: Res<Counts>| assert_eq!(counts.0.packets, 1))
                    .after(SystemSets::ClientReceive),
            ),
        );
        fixture.enqueue();
        fixture.app.update();
    }

    #[test]
    fn zero_budget_pauses_and_each_frame_counts_all_event_types() {
        let mut fixture = Fixture::new(256, 2, 2, Scenario::Interleaved);
        fixture.enqueue();
        fixture
            .app
            .world_mut()
            .resource_mut::<NetworkQueueSettings>()
            .events_per_frame = 0;
        fixture.app.update();
        assert_eq!(
            fixture.app.world().resource::<Counts>().0,
            Delivery::default()
        );
        fixture
            .app
            .world_mut()
            .resource_mut::<NetworkQueueSettings>()
            .events_per_frame = 3;
        for expected in [3, 6, 8] {
            fixture.app.update();
            let counts = fixture.app.world().resource::<Counts>().0;
            assert_eq!(
                counts.established + counts.packets + counts.closed,
                expected
            );
        }
    }
}
