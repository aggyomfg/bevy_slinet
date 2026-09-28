//! Sends independent packets over unreliable, unordered SLN2 datagrams.
//!
//! Serializers must tolerate loss, reordering and malformed payloads; length serializers are unused.
//! The cookie handshake validates return addresses before allocating peers, without authentication
//! or encryption. Session identifiers isolate replacement connections from delayed traffic.
//!
//! Use [`ConfiguredUdpProtocol`] with [`UdpConfig`] for transport limits and [`UdpIdleTimeout`]
//! for app-local liveness. Both endpoints must use the same wire version.

mod diagnostics;
mod handshake;
mod listener;
mod pacing;
mod session;
mod settings;
mod stream;
mod wire;

pub use diagnostics::{UdpConnectionHandle, UdpStatsSnapshot};

pub use listener::UdpNetworkListener;
pub use settings::{
    ConfiguredUdpProtocol, DefaultUdpConfig, UdpConfig, UdpIdleTimeout, UdpOptions, UdpProtocol,
    CONNECT_TIMEOUT, KEEPALIVE_INTERVAL, MAX_DATAGRAM_SIZE, PROBE_INTERVAL,
};
pub use stream::{
    ConfiguredUdpClientStream, UdpClientStream, UdpReadHalf, UdpServerStream, UdpWriteHalf,
};

#[cfg(any(feature = "client", feature = "server"))]
pub(crate) use settings::IdleTimeoutSettings;

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;

#[cfg(test)]
mod data_tests;
#[cfg(test)]
mod handshake_tests;
