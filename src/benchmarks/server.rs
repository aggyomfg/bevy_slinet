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
use crate::bench_support::{Delivery, Scenario};
use crate::protocols::tcp::TcpProtocol;
use crate::serializers::{
    bitcode::BitcodeSerializer, packet_length_serializer::LittleEndian,
    serializer::SerializerAdapter,
};
use std::convert::Infallible;
use std::sync::atomic::AtomicBool;

struct Config;
impl ServerConfig for Config {
    type ClientPacket = u64;
    type ServerPacket = u64;
    type Protocol = TcpProtocol;
    type EncodeError = Infallible;
    type DecodeError = bitcode::Error;
    type LengthSerializer = LittleEndian<u32>;
    fn build_serializer() -> SerializerAdapter<u64, u64, Infallible, bitcode::Error> {
        SerializerAdapter::ReadOnly(Arc::new(BitcodeSerializer))
    }
}
#[derive(Resource, Default)]
struct Counts(Delivery);

pub struct Fixture {
    app: App,
    sender: Sender<ServerLifecycle<Config>>,
    connections: Vec<ServerConnection<Config>>,
    _outgoing: Vec<OutgoingReceiver<u64, ()>>,
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
        assert!(budget > 0 && peers > 0 && packets_per_connection > 0);
        let settings = NetworkQueueSettings {
            receive_capacity: peers * (packets_per_connection + 2),
            events_per_frame: budget,
            ..Default::default()
        };
        let (sender, receiver) = settings.incoming_channel();
        let (_, packets) =
            lossy_channel(1, usize::MAX, crate::connection::OverflowPolicy::DropNewest);
        let mut app = App::new();
        app.insert_resource(settings)
            .insert_resource(LifecycleReceiver::<Config>(receiver.into()))
            .insert_resource(PacketReceiver::<Config> { receiver: packets })
            .insert_resource(ServerConnections::<Config>::new())
            .init_resource::<Counts>()
            .add_systems(
                PreUpdate,
                (lifecycle_system::<Config>, accept_new_packets::<Config>).chain(),
            )
            .add_observer(
                |_: On<NewConnectionEvent<Config>>, mut counts: ResMut<Counts>| {
                    counts.0.established += 1;
                },
            )
            .add_observer(
                |_: On<PacketReceiveEvent<Config>>, mut counts: ResMut<Counts>| {
                    counts.0.packets += 1;
                    counts.0.last_packet_frame = counts.0.frames;
                },
            )
            .add_observer(
                |_: On<DisconnectionEvent<Config>>, mut counts: ResMut<Counts>| {
                    counts.0.closed += 1;
                    counts.0.last_close_frame = counts.0.frames;
                },
            );
        app.update(); // Initialize schedules outside measurements.
        let mut connections = Vec::new();
        let mut outgoing = Vec::new();
        for _ in 0..peers {
            let (packet_tx, rx) = settings.outgoing_channel(false);
            outgoing.push(rx);
            connections.push(EcsConnection {
                disconnect_task: CancellationToken::new(),
                id: ConnectionId::next(),
                published: Arc::new(AtomicBool::new(false)),
                packet_tx,
                transport: (),
                local_addr: ([127, 0, 0, 1], 1000).into(),
                peer_addr: ([127, 0, 0, 1], 2000).into(),
            });
        }
        let mut result = Self {
            app,
            sender,
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
    fn establish(&self, connection: &ServerConnection<Config>) {
        assert!(self
            .sender
            .try_send(ServerLifecycle::Established(NewConnectionEvent {
                address: connection.peer_addr(),
                connection: connection.clone()
            }))
            .is_ok());
    }
    /// Enqueue outside the timing/allocation region; all events fit without blocking.
    pub fn enqueue(&mut self) {
        self.app.world_mut().resource_mut::<Counts>().0 = Delivery::default();
        for connection in &self.connections {
            if matches!(self.scenario, Scenario::Interleaved) {
                self.establish(connection);
            }
            for packet in 0..self.packets_per_connection {
                assert!(self
                    .sender
                    .try_send(ServerLifecycle::Packet(PacketReceiveEvent {
                        connection: connection.clone(),
                        packet: packet as u64,
                        received_at: Instant::now(),
                    }))
                    .is_ok());
            }
            if matches!(self.scenario, Scenario::Interleaved) {
                let event = DisconnectionEvent {
                    error: ReceiveError::IntentionalDisconnection,
                    connection: connection.clone(),
                };
                assert!(self.sender.try_send(ServerLifecycle::Closed(event)).is_ok());
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
            if result.packets == expected_packets && result.closed == expected_closed {
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
    fn frame_budget_and_phase_boundaries_are_visible() {
        for (scenario, budget, frames) in [
            (Scenario::Packets, 1, 8),
            (Scenario::Packets, 256, 1),
            (Scenario::Interleaved, 256, 9),
        ] {
            let mut fixture = Fixture::new(budget, 8, 1, scenario);
            fixture.enqueue();
            let result = fixture.drain();
            assert_eq!(result.packets, 8);
            assert_eq!(result.frames, frames);
            if matches!(scenario, Scenario::Interleaved) {
                assert_eq!(result.established, 8);
                assert_eq!(result.closed, 8);
                assert_eq!(result.last_packet_frame, 8);
                assert_eq!(result.last_close_frame, 9);
            }
            fixture.enqueue();
            assert_eq!(fixture.drain(), result);
        }
    }
}
