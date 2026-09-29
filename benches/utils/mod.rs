//! Internal benchmark fixtures. No compatibility guarantees.
#![allow(missing_docs, clippy::missing_errors_doc, clippy::must_use_candidate)]
pub use crate::client::bench_utils::Fixture as ClientFixture;
pub type UdpClientFixture = ClientFixture<crate::protocols::udp::UdpProtocol>;
pub use crate::packet_queue::{lossy_channel, LossyReceiver as Receiver, LossySender as Sender};
pub use crate::protocols::udp::bench_utils::RawPeer;
pub use crate::server::bench_utils::Fixture as ServerFixture;

#[derive(Clone, Copy, Debug)]
pub enum Scenario {
    Packets,
    Interleaved,
}
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delivery {
    pub frames: usize,
    pub packets: usize,
    pub established: usize,
    pub closed: usize,
    pub last_packet_frame: usize,
    pub last_close_frame: usize,
}
