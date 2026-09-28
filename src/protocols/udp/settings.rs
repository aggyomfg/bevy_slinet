use super::wire::HEADER;
use super::{ConfiguredUdpClientStream, UdpNetworkListener, UdpServerStream};
use crate::Protocol;
use async_trait::async_trait;
#[cfg(any(feature = "client", feature = "server"))]
use bevy::log;
use bevy::prelude::Resource;
use std::{
    io::{self, ErrorKind},
    marker::PhantomData,
    net::SocketAddr,
    time::Duration,
};
#[cfg(any(feature = "client", feature = "server"))]
use tokio::sync::watch;
use tokio::{sync::Semaphore, time::Instant as Clock};

pub(super) const BUFFER_SIZE: usize = u16::MAX as usize;
/// Absolute UDP datagram ceiling supported on both IPv4 and IPv6.
pub const MAX_DATAGRAM_SIZE: usize = 65_507;
/// Default heartbeat interval.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);
/// Retry interval for both handshake legs.
pub const PROBE_INTERVAL: Duration = Duration::from_millis(250);
/// Total timeout for the cookie handshake.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const DISCONNECT_REPEATS: usize = 3;
pub(super) const MAX_QUEUED_DATAGRAMS: usize = 1024;
pub(super) const MAX_QUEUED_BYTES: usize = 1 << 20;

/// UDP admission, outgoing packet size and heartbeat settings.
#[derive(Clone, Copy, Debug)]
pub struct UdpOptions {
    /// Maximum registered peers, including accepted streams waiting to be split.
    pub max_peers: usize,
    /// Maximum HELLO/CONFIRM packets handled per second, shared by all addresses.
    pub max_handshake_packets_per_second: usize,
    /// Maximum outgoing data datagram size, including the 37-byte session header.
    /// Defaults to 1200 bytes. Configure this for the path MTU; it is not PMTU discovery.
    pub max_datagram_size: usize,
    /// Heartbeat interval while no application data is being sent.
    pub heartbeat_interval: Duration,
    /// Additional random delay in [0, heartbeat_jitter] for each heartbeat tick.
    pub heartbeat_jitter: Duration,
}
impl UdpOptions {
    /// Default UDP settings.
    pub const DEFAULT: Self = Self {
        max_peers: 1024,
        max_handshake_packets_per_second: 256,
        max_datagram_size: 1200,
        heartbeat_interval: KEEPALIVE_INTERVAL,
        heartbeat_jitter: Duration::from_millis(100),
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
    type Listener = UdpNetworkListener;
    type ServerStream = UdpServerStream;
    type ClientStream = ConfiguredUdpClientStream<C>;
    const DATAGRAM: bool = true;
    async fn bind(addr: SocketAddr) -> io::Result<Self::Listener> {
        UdpNetworkListener::bind(addr, C::OPTIONS).await
    }
}
/// Closes a UDP connection when no datagrams arrive from the peer for this long.
/// Allow several heartbeat intervals (including jitter); `Duration::MAX` disables it.
/// Defaults to 10 seconds. Applies only to this Bevy app. Changes wake pending reads;
/// the deadline is measured from the last valid datagram. Removing the resource restores the default.
#[derive(Clone, Copy, Debug, Resource)]
pub struct UdpIdleTimeout(pub Duration);

impl Default for UdpIdleTimeout {
    fn default() -> Self {
        UdpIdleTimeout(Duration::from_secs(10))
    }
}

#[cfg(any(feature = "client", feature = "server"))]
#[derive(Resource)]
pub(crate) struct IdleTimeoutSettings(tokio::sync::watch::Sender<Duration>);

#[cfg(any(feature = "client", feature = "server"))]
impl Default for IdleTimeoutSettings {
    fn default() -> Self {
        Self(tokio::sync::watch::channel(UdpIdleTimeout::default().0).0)
    }
}

/// Adds the systems that forward [`UdpIdleTimeout`] to this app's connections.
#[cfg(any(feature = "client", feature = "server"))]
impl IdleTimeoutSettings {
    pub(crate) fn install(app: &mut bevy::prelude::App) -> watch::Receiver<Duration> {
        use bevy::prelude::{Startup, Update};

        if !app.world().contains_resource::<IdleTimeoutSettings>() {
            app.init_resource::<IdleTimeoutSettings>()
                .add_systems(Startup, set_idle_timeout_system)
                .add_systems(Update, set_idle_timeout_system);
        }
        app.world().resource::<IdleTimeoutSettings>().0.subscribe()
    }
}

#[cfg(any(feature = "client", feature = "server"))]
pub(crate) fn set_idle_timeout_system(
    timeout: Option<bevy::prelude::Res<UdpIdleTimeout>>,
    settings: bevy::prelude::Res<IdleTimeoutSettings>,
) {
    let timeout = timeout.map(|timeout| *timeout).unwrap_or_default().0;
    settings.0.send_if_modified(|current| {
        if *current == timeout {
            return false;
        }
        if timeout.is_zero() {
            log::warn!("UdpIdleTimeout is zero; connections expire immediately");
        }
        *current = timeout;
        true
    });
}

#[derive(Clone, Copy, Debug)]
pub(super) struct ValidatedOptions(UdpOptions);
impl ValidatedOptions {
    pub(super) fn new(options: UdpOptions) -> io::Result<Self> {
        let delay = options
            .heartbeat_interval
            .checked_add(options.heartbeat_jitter);
        if !(HEADER..=MAX_DATAGRAM_SIZE).contains(&options.max_datagram_size)
            || options.max_peers > Semaphore::MAX_PERMITS
            || options.heartbeat_interval.is_zero()
            || delay
                .and_then(|value| Clock::now().checked_add(value))
                .is_none()
        {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "invalid UDP size or heartbeat settings",
            ));
        }
        Ok(Self(options))
    }
    pub(super) fn max_peers(self) -> usize {
        self.0.max_peers
    }
    pub(super) fn max_handshake_packets_per_second(self) -> usize {
        self.0.max_handshake_packets_per_second
    }
    pub(super) fn max_datagram_size(self) -> usize {
        self.0.max_datagram_size
    }
    pub(super) fn heartbeat_interval(self) -> Duration {
        self.0.heartbeat_interval
    }
    pub(super) fn heartbeat_jitter(self) -> Duration {
        self.0.heartbeat_jitter
    }
}
