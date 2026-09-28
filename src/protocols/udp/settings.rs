use super::{ConfiguredUdpClientStream, UdpConnectionHandle, UdpNetworkListener, UdpServerStream};
use crate::{connection::OverflowPolicy, Protocol};
use async_trait::async_trait;
use std::{io, marker::PhantomData, net::SocketAddr, num::NonZeroU64};
use tokio::sync::Semaphore;
pub(super) const BUFFER_SIZE: usize = u16::MAX as usize;
/// Maximum supported UDP payload on both IPv4 and IPv6.
pub const MAX_DATAGRAM_SIZE: usize = 65_507;
/// UDP resource limits. There is no library wire header or remote lifecycle protocol.
#[derive(Clone, Copy, Debug)]
pub struct UdpOptions {
    /// Maximum local peers, including unsplit streams.
    pub max_peers: usize,
    /// Maximum outgoing serialized payload size. No library bytes are added.
    pub max_datagram_size: usize,
    /// Maximum datagrams queued per server peer.
    pub receive_queue_capacity: usize,
    /// Maximum serialized bytes queued per server peer.
    pub receive_queue_bytes: usize,
    /// Raw datagram queue overflow policy.
    pub receive_queue_overflow: OverflowPolicy,
    /// Optional outgoing serialized bytes per second. Not congestion control.
    pub send_rate: Option<NonZeroU64>,
}
impl UdpOptions {
    /// Maximum application payload; zero allows only empty datagrams.
    #[must_use]
    pub const fn max_payload_size(self) -> Option<usize> {
        if self.max_datagram_size > MAX_DATAGRAM_SIZE {
            None
        } else {
            Some(self.max_datagram_size)
        }
    }
    /// Defaults suitable for small packets and bounded local queues.
    pub const DEFAULT: Self = Self {
        max_peers: 1024,
        max_datagram_size: 1200,
        receive_queue_capacity: 1024,
        receive_queue_bytes: 1 << 20,
        receive_queue_overflow: OverflowPolicy::DropNewest,
        send_rate: None,
    };
}
impl Default for UdpOptions {
    fn default() -> Self {
        Self::DEFAULT
    }
}
/// Supplies UDP options for one protocol/plugin configuration.
pub trait UdpConfig: Send + Sync + 'static {
    /// Options used when binding or connecting.
    const OPTIONS: UdpOptions = UdpOptions::DEFAULT;
}
/// Default UDP configuration.
pub struct DefaultUdpConfig;
impl UdpConfig for DefaultUdpConfig {}
/// UDP protocol with custom settings.
pub struct ConfiguredUdpProtocol<C: UdpConfig>(PhantomData<C>);
/// UDP protocol with the default settings.
pub type UdpProtocol = ConfiguredUdpProtocol<DefaultUdpConfig>;

#[async_trait]
impl<C: UdpConfig> Protocol for ConfiguredUdpProtocol<C> {
    type Handle = UdpConnectionHandle;
    type Listener = UdpNetworkListener;
    type ServerStream = UdpServerStream;
    type ClientStream = ConfiguredUdpClientStream<C>;
    const DATAGRAM: bool = true;
    async fn bind(addr: SocketAddr) -> io::Result<Self::Listener> {
        UdpNetworkListener::bind(addr, C::OPTIONS).await
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ValidatedOptions(UdpOptions);
impl ValidatedOptions {
    pub(super) fn new(options: UdpOptions) -> io::Result<Self> {
        if options.max_payload_size().is_none() || options.max_peers > Semaphore::MAX_PERMITS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid UDP transport settings",
            ));
        }
        Ok(Self(options))
    }
    pub(super) const fn max_peers(self) -> usize {
        self.0.max_peers
    }
    pub(super) const fn receive_queue_capacity(self) -> usize {
        self.0.receive_queue_capacity
    }
    pub(super) const fn receive_queue_bytes(self) -> usize {
        self.0.receive_queue_bytes
    }
    pub(super) const fn receive_queue_overflow(self) -> OverflowPolicy {
        self.0.receive_queue_overflow
    }
    pub(super) const fn send_rate(self) -> Option<NonZeroU64> {
        self.0.send_rate
    }
    pub(super) const fn max_payload_size(self) -> usize {
        self.0.max_datagram_size
    }
}
