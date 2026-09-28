use super::wire::HEADER;
use super::{ConfiguredUdpClientStream, UdpNetworkListener, UdpServerStream};
use crate::{connection::OverflowPolicy, Protocol};
use async_trait::async_trait;
use bevy::prelude::Resource;
#[cfg(any(feature = "client", feature = "server"))]
use bevy::{ecs::system::SystemParam, log, prelude::Res};
use std::{
    io::{self, ErrorKind},
    marker::PhantomData,
    net::SocketAddr,
    num::NonZeroU64,
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
/// Default initial retry interval for each handshake leg.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(1);
/// Total timeout for the cookie handshake.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) const DISCONNECT_REPEATS: usize = 3;
pub(super) const MAX_QUEUED_DATAGRAMS: usize = 1024;
pub(super) const MAX_QUEUED_BYTES: usize = 1 << 20;

/// UDP handshake, session traffic, pacing and raw queue settings.
#[derive(Clone, Copy, Debug)]
pub struct UdpOptions {
    /// Maximum registered peers, including accepted streams waiting to be split.
    pub max_peers: usize,
    /// Maximum HELLO/CONFIRM packets handled per second, shared by all addresses.
    pub max_handshake_packets_per_second: usize,
    /// Maximum address replay watermarks retained until their cookies expire.
    pub max_replay_entries: usize,
    /// Maximum outgoing data datagram size, including the 37-byte session header.
    /// Configure this for the path MTU; it does not perform PMTU discovery.
    pub max_datagram_size: usize,
    /// Absolute deadline for both client handshake legs.
    pub connect_timeout: Duration,
    /// Delay before the first retry of each handshake leg.
    pub initial_retry_interval: Duration,
    /// Maximum base delay between handshake retries.
    pub max_retry_interval: Duration,
    /// Additional random delay in [0, `retry_jitter`] for each retry.
    pub retry_jitter: Duration,
    /// Maximum count of queued raw datagrams per accepted peer.
    pub receive_queue_capacity: usize,
    /// Maximum combined bytes in the raw datagram queue per accepted peer.
    pub receive_queue_bytes: usize,
    /// Overflow behavior for the raw datagram queue.
    pub receive_queue_overflow: OverflowPolicy,
    /// Optional byte rate limit for outgoing UDP data datagrams.
    pub send_rate: Option<NonZeroU64>,
    /// Heartbeat interval while no application data is being sent.
    pub heartbeat_interval: Duration,
    /// Additional random delay in [0, `heartbeat_jitter`] for each heartbeat tick.
    pub heartbeat_jitter: Duration,
}
impl UdpOptions {
    /// Maximum application payload that fits in a configured datagram.
    /// Returns `None` when the datagram size is outside the supported range.
    #[must_use]
    pub const fn max_payload_size(self) -> Option<usize> {
        if self.max_datagram_size < HEADER || self.max_datagram_size > MAX_DATAGRAM_SIZE {
            None
        } else {
            Some(self.max_datagram_size - HEADER)
        }
    }

    /// Provides a starting point for compile-time overrides in [`UdpConfig::OPTIONS`].
    pub const DEFAULT: Self = Self {
        max_peers: 1024,
        max_handshake_packets_per_second: 256,
        max_replay_entries: 16_384,
        max_datagram_size: 1200,
        connect_timeout: CONNECT_TIMEOUT,
        initial_retry_interval: PROBE_INTERVAL,
        max_retry_interval: Duration::from_secs(4),
        retry_jitter: Duration::from_millis(100),
        receive_queue_capacity: MAX_QUEUED_DATAGRAMS,
        receive_queue_bytes: MAX_QUEUED_BYTES,
        receive_queue_overflow: OverflowPolicy::DropNewest,
        send_rate: None,
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
        let retry_delay = options.max_retry_interval.checked_add(options.retry_jitter);
        if !(HEADER..=MAX_DATAGRAM_SIZE).contains(&options.max_datagram_size)
            || options.max_peers > Semaphore::MAX_PERMITS
            || options.max_replay_entries == 0
            || options.connect_timeout.is_zero()
            || options.initial_retry_interval.is_zero()
            || options.max_retry_interval < options.initial_retry_interval
            || retry_delay
                .and_then(|value| Clock::now().checked_add(value))
                .is_none()
            || Clock::now().checked_add(options.connect_timeout).is_none()
            || options.heartbeat_interval.is_zero()
            || delay
                .and_then(|value| Clock::now().checked_add(value))
                .is_none()
        {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "invalid UDP transport settings",
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
    pub(super) const fn max_replay_entries(self) -> usize {
        self.0.max_replay_entries
    }
    pub(super) const fn connect_timeout(self) -> Duration {
        self.0.connect_timeout
    }
    pub(super) const fn initial_retry_interval(self) -> Duration {
        self.0.initial_retry_interval
    }
    pub(super) const fn max_retry_interval(self) -> Duration {
        self.0.max_retry_interval
    }
    pub(super) const fn retry_jitter(self) -> Duration {
        self.0.retry_jitter
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
        self.0.max_datagram_size - HEADER
    }
    pub(super) const fn heartbeat_interval(self) -> Duration {
        self.0.heartbeat_interval
    }
    pub(super) const fn heartbeat_jitter(self) -> Duration {
        self.0.heartbeat_jitter
    }
}
