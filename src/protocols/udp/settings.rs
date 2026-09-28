use super::wire::HEADER;
use super::{ConfiguredUdpClientStream, UdpNetworkListener, UdpServerStream};
use crate::Protocol;
use async_trait::async_trait;
use bevy::prelude::Resource;
#[cfg(any(feature = "client", feature = "server"))]
use bevy::{ecs::system::SystemParam, log, prelude::Res};
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
    /// Configure this for the path MTU; it does not perform PMTU discovery.
    pub max_datagram_size: usize,
    /// Heartbeat interval while no application data is being sent.
    pub heartbeat_interval: Duration,
    /// Additional random delay in [0, `heartbeat_jitter`] for each heartbeat tick.
    pub heartbeat_jitter: Duration,
}
impl UdpOptions {
    /// Provides a starting point for compile-time overrides in [`UdpConfig::OPTIONS`].
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
/// Closes app-local sessions after this much time without a valid peer datagram.
///
/// Changes wake pending reads; removing this resource restores the default timeout.
/// `Duration::MAX` disables expiry. Allow several heartbeat intervals, including jitter.
#[derive(Clone, Copy, Debug, Resource)]
pub struct UdpIdleTimeout(pub Duration);

impl Default for UdpIdleTimeout {
    fn default() -> Self {
        Self(Duration::from_secs(10))
    }
}

#[cfg(any(feature = "client", feature = "server"))]
#[derive(Resource)]
pub struct IdleTimeoutSettings(tokio::sync::watch::Sender<Duration>);

#[cfg(any(feature = "client", feature = "server"))]
impl Default for IdleTimeoutSettings {
    fn default() -> Self {
        Self(tokio::sync::watch::channel(UdpIdleTimeout::default().0).0)
    }
}

#[cfg(any(feature = "client", feature = "server"))]
impl IdleTimeoutSettings {
    /// Forwards [`UdpIdleTimeout`] changes to this app's connections.
    pub(crate) fn install(app: &mut bevy::prelude::App) -> watch::Receiver<Duration> {
        use bevy::prelude::{Startup, Update};

        if !app.world().contains_resource::<Self>() {
            app.init_resource::<Self>()
                .add_systems(Startup, set_idle_timeout_system)
                .add_systems(Update, set_idle_timeout_system);
        }
        app.world().resource::<Self>().0.subscribe()
    }
}

#[cfg(any(feature = "client", feature = "server"))]
/// Reads the app's timeout override and publishes it to active connections.
#[derive(SystemParam)]
struct IdleTimeouts<'w> {
    timeout: Option<Res<'w, UdpIdleTimeout>>,
    settings: Res<'w, IdleTimeoutSettings>,
}

#[cfg(any(feature = "client", feature = "server"))]
impl IdleTimeouts<'_> {
    fn synchronize(&self) {
        let timeout = self.timeout.as_deref().copied().unwrap_or_default().0;
        self.settings.0.send_if_modified(|current| {
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
}

#[cfg(any(feature = "client", feature = "server"))]
fn set_idle_timeout_system(timeouts: IdleTimeouts) {
    timeouts.synchronize();
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
    pub(super) const fn max_peers(self) -> usize {
        self.0.max_peers
    }
    pub(super) const fn max_handshake_packets_per_second(self) -> usize {
        self.0.max_handshake_packets_per_second
    }
    pub(super) const fn max_payload_size(self) -> usize {
        self.0.max_datagram_size - HEADER
    }
    pub(super) const fn heartbeat_interval(self) -> Duration {
        self.0.heartbeat_interval
    }
    pub(super) const fn heartbeat_jitter(self) -> Duration {
        self.0.heartbeat_jitter
    }
}
