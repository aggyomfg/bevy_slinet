//! Independent UDP datagrams containing exactly the application serializer output.
//!
//! Local connections associate addresses and queues; they do not validate remote peers.
//! Applications implement handshake, session IDs, heartbeat and remote close as needed.
//! Client setup sends nothing; a server peer is created by its first datagram.
//! Closing a local peer sends nothing. Serializers must tolerate loss and reordering.

mod diagnostics;
mod listener;
mod pacing;
mod peer;
mod settings;
mod stream;

pub use diagnostics::{UdpConnectionHandle, UdpStatsSnapshot};

pub use listener::UdpNetworkListener;
pub use settings::{
    ConfiguredUdpProtocol, DefaultUdpConfig, UdpConfig, UdpOptions, UdpProtocol, MAX_DATAGRAM_SIZE,
};
pub use stream::{
    ConfiguredUdpClientStream, UdpClientStream, UdpReadHalf, UdpServerStream, UdpWriteHalf,
};

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod data_tests;

#[cfg(test)]
mod contract_tests;

#[cfg(feature = "bench-internals")]
#[doc(hidden)]
pub mod bench_support;
