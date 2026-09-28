//! UDP transport with one application packet per versioned, session-tagged datagram.
//!
//! Delivery is unreliable and unordered. Serializers must decode packets independently,
//! including after malformed input. `LengthSerializer` is unused; empty payloads are supported.
//! The SLN2 handshake validates the return address with a short-lived stateless cookie before
//! allocating a peer. Cookies do not authenticate users or encrypt traffic. All data and control
//! packets carry a session identifier, so delayed packets cannot affect a replacement session.
//! Both peers must use the same wire version; older UDP framing is incompatible.
//!
//! [`UdpProtocol`] uses default options. Use [`ConfiguredUdpProtocol`] and [`UdpConfig`] for
//! per-plugin limits. [`UdpIdleTimeout`] controls liveness in each Bevy app. Control replies
//! never wait for socket writability; lost replies are recovered by retries or idle timeouts.

mod wire;
use crate::connection::MAX_PACKET_SIZE;
use crate::protocol::{
    ClientStream, Listener, NetworkStream, ReadStream, ReceiveError, ServerStream, WriteStream,
};
use crate::serializer::Serializer;
use crate::{PacketLengthSerializer, Protocol};
use async_trait::async_trait;
use bevy::{log, platform::time::Instant, prelude::Resource};
use std::collections::HashMap;
use std::fmt::Debug;
use std::io::{self, ErrorKind};
use std::marker::PhantomData;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::{mpsc, watch, OwnedSemaphorePermit, Semaphore};
use tokio::time::{Instant as Clock, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use wire::*;

const BUFFER_SIZE: usize = u16::MAX as usize;
/// Absolute UDP datagram ceiling supported on both IPv4 and IPv6.
pub const MAX_DATAGRAM_SIZE: usize = 65_507;
/// Default heartbeat interval.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);
/// Retry interval for both handshake legs.
pub const PROBE_INTERVAL: Duration = Duration::from_millis(250);
/// Total timeout for the cookie handshake.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const DISCONNECT_REPEATS: usize = 3;
const MAX_QUEUED_DATAGRAMS: usize = 1024;
const MAX_QUEUED_BYTES: usize = 1 << 20;

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
    fn validate(self) -> io::Result<Self> {
        let delay = self.heartbeat_interval.checked_add(self.heartbeat_jitter);
        if !(HEADER..=MAX_DATAGRAM_SIZE).contains(&self.max_datagram_size)
            || self.max_peers > Semaphore::MAX_PERMITS
            || self.heartbeat_interval.is_zero()
            || delay
                .and_then(|value| Clock::now().checked_add(value))
                .is_none()
        {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "invalid UDP size or heartbeat settings",
            ));
        }
        Ok(self)
    }
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
pub(crate) fn idle_timeout_receiver(app: &mut bevy::prelude::App) -> watch::Receiver<Duration> {
    use bevy::prelude::{Startup, Update};

    if !app.world().contains_resource::<IdleTimeoutSettings>() {
        app.init_resource::<IdleTimeoutSettings>()
            .add_systems(Startup, set_idle_timeout_system)
            .add_systems(Update, set_idle_timeout_system);
    }
    app.world().resource::<IdleTimeoutSettings>().0.subscribe()
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

type Queued = (Box<[u8]>, Instant);
type Peers = Mutex<HashMap<SocketAddr, Peer>>;

struct SessionState {
    id: Session,
    generation: u64,
    options: UdpOptions,
    queued_bytes: AtomicUsize,
    seen: AtomicBool,
    response_credits: AtomicUsize,
    last_received: Mutex<Clock>,
    last_sent: Mutex<Clock>,
    dropped_oversized: AtomicUsize,
    dropped_send_errors: AtomicUsize,
    closed: CancellationToken,
}
impl SessionState {
    fn new(cookie: Cookie, options: UdpOptions) -> Arc<Self> {
        Arc::new(Self {
            id: cookie.mac,
            generation: cookie.generation,
            options,
            queued_bytes: AtomicUsize::new(0),
            seen: AtomicBool::new(false),
            response_credits: AtomicUsize::new(0),
            last_received: Mutex::new(Clock::now()),
            last_sent: Mutex::new(Clock::now()),
            dropped_oversized: AtomicUsize::new(0),
            dropped_send_errors: AtomicUsize::new(0),
            closed: CancellationToken::new(),
        })
    }
    fn received(&self) {
        *self.last_received.lock().unwrap() = Clock::now();
        self.seen.store(true, Ordering::Relaxed);
        let _ =
            self.response_credits
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |credit| {
                    Some((credit + 1).min(DISCONNECT_REPEATS + 1))
                });
    }
    fn take_credit(&self) -> bool {
        self.response_credits
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |credit| {
                credit.checked_sub(1)
            })
            .is_ok()
    }
}

struct Peer {
    queue: mpsc::Sender<Queued>,
    state: Arc<SessionState>,
}
impl Peer {
    fn push(&self, bytes: &[u8], received_at: Instant) {
        if self.state.queued_bytes.load(Ordering::Relaxed) + bytes.len() > MAX_QUEUED_BYTES {
            return;
        }
        if let Ok(permit) = self.queue.try_reserve() {
            self.state
                .queued_bytes
                .fetch_add(bytes.len(), Ordering::Relaxed);
            permit.send((bytes.into(), received_at));
        }
    }
}

struct PeerRegistration {
    _slot: OwnedSemaphorePermit,
    peers: Weak<Peers>,
    address: SocketAddr,
    state: Arc<SessionState>,
}
impl Drop for PeerRegistration {
    fn drop(&mut self) {
        if let Some(peers) = self.peers.upgrade() {
            let mut peers = peers.lock().unwrap();
            if peers
                .get(&self.address)
                .is_some_and(|peer| Arc::ptr_eq(&peer.state, &self.state))
            {
                peers.remove(&self.address);
            }
        }
        self.state.closed.cancel();
    }
}

struct Admission {
    started: Clock,
    used: usize,
}
impl Admission {
    fn take(&mut self, limit: usize) -> bool {
        if self.started.elapsed() >= Duration::from_secs(1) {
            self.started = Clock::now();
            self.used = 0;
        }
        if self.used >= limit {
            return false;
        }
        self.used += 1;
        true
    }
}

/// A shared UDP socket that routes packets to validated sessions.
pub struct UdpNetworkListener {
    socket: Arc<UdpSocket>,
    peers: Arc<Peers>,
    cookies: CookieJar,
    options: UdpOptions,
    admission: Mutex<Admission>,
    slots: Arc<Semaphore>,
}
impl UdpNetworkListener {
    async fn bind(address: SocketAddr, options: UdpOptions) -> io::Result<Self> {
        let options = options.validate()?;
        Ok(Self {
            slots: Arc::new(Semaphore::new(options.max_peers)),
            socket: Arc::new(UdpSocket::bind(address).await?),
            peers: Arc::default(),
            cookies: CookieJar::new()?,
            options,
            admission: Mutex::new(Admission {
                started: Clock::now(),
                used: 0,
            }),
        })
    }

    // Synchronous dispatch keeps all control sends out of the socket receive wait path.
    fn dispatch(
        &self,
        bytes: &[u8],
        address: SocketAddr,
        received_at: Instant,
    ) -> Option<UdpServerStream> {
        if let Some((tag, cookie)) = Cookie::parse(bytes) {
            if !matches!(tag, HELLO | CONFIRM)
                || !self
                    .admission
                    .lock()
                    .unwrap()
                    .take(self.options.max_handshake_packets_per_second)
            {
                return None;
            }
            if tag == HELLO {
                let challenge = self.cookies.issue(address, cookie.nonce).encode(CHALLENGE);
                let _ = self.socket.try_send_to(&challenge, address);
                return None;
            }
            let mut peers = self.peers.lock().unwrap();
            if let Some(peer) = peers.get(&address) {
                if peer.state.id == cookie.mac {
                    let reply = if peer.state.closed.is_cancelled() {
                        DISCONNECT
                    } else {
                        ACCEPT
                    };
                    let _ = self
                        .socket
                        .try_send_to(&control(reply, &cookie.mac), address);
                    return None;
                }
                // A delayed confirmation must not replace a newer session.
                if cookie.generation <= peer.state.generation {
                    return None;
                }
            }
            if !self.cookies.verify(address, &cookie) {
                return None;
            }
            let Ok(slot) = Arc::clone(&self.slots).try_acquire_owned() else {
                let _ = self
                    .socket
                    .try_send_to(&control(DISCONNECT, &cookie.mac), address);
                return None;
            };
            let state = SessionState::new(cookie, self.options);
            let (queue, incoming) = mpsc::channel(MAX_QUEUED_DATAGRAMS);
            if let Some(old) = peers.insert(
                address,
                Peer {
                    queue,
                    state: Arc::clone(&state),
                },
            ) {
                old.state.closed.cancel();
            }
            let _ = self
                .socket
                .try_send_to(&control(ACCEPT, &state.id), address);
            return Some(UdpServerStream {
                registration: PeerRegistration {
                    _slot: slot,
                    peers: Arc::downgrade(&self.peers),
                    address,
                    state: Arc::clone(&state),
                },
                incoming,
                state,
                peer_addr: address,
                socket: Arc::clone(&self.socket),
            });
        }
        if let Some((tag, session, _)) = parse_frame(bytes) {
            if tag == ACCEPT {
                return None;
            }
            let peers = self.peers.lock().unwrap();
            match peers.get(&address).filter(|peer| peer.state.id == session) {
                Some(peer) => {
                    peer.state.received();
                    match tag {
                        DISCONNECT => peer.state.closed.cancel(),
                        DATA => peer.push(bytes, received_at),
                        _ => {}
                    }
                }
                None if tag != DISCONNECT => {
                    let _ = self
                        .socket
                        .try_send_to(&control(DISCONNECT, &session), address);
                }
                None => {}
            }
        }
        None
    }
}
#[async_trait]
impl Listener for UdpNetworkListener {
    type Stream = UdpServerStream;
    async fn accept(&self) -> io::Result<Self::Stream> {
        let mut buffer = vec![0; BUFFER_SIZE];
        loop {
            let (len, address) = match self.socket.recv_from(&mut buffer).await {
                Ok(value) => value,
                Err(err)
                    if matches!(
                        err.kind(),
                        ErrorKind::ConnectionReset | ErrorKind::ConnectionRefused
                    ) =>
                {
                    continue
                }
                Err(err) => return Err(err),
            };
            if let Some(stream) = self.dispatch(&buffer[..len], address, Instant::now()) {
                return Ok(stream);
            }
        }
    }
    fn address(&self) -> SocketAddr {
        self.socket.local_addr().unwrap()
    }
}

enum Incoming {
    Queue {
        queue: mpsc::Receiver<Queued>,
        current: Box<[u8]>,
        _registration: PeerRegistration,
    },
    Socket {
        socket: Arc<UdpSocket>,
        buffer: Box<[u8]>,
    },
}
impl Incoming {
    async fn next(&mut self, state: &SessionState) -> io::Result<(&[u8], Instant)> {
        match self {
            Self::Queue { queue, current, .. } => {
                let (bytes, received_at) = queue.recv().await.ok_or_else(disconnected_error)?;
                state.queued_bytes.fetch_sub(bytes.len(), Ordering::Relaxed);
                *current = bytes;
                Ok((current, received_at))
            }
            Self::Socket { socket, buffer } => {
                let len = socket.recv(buffer).await?;
                Ok((&buffer[..len], Instant::now()))
            }
        }
    }
}

fn try_control(socket: &UdpSocket, address: Option<SocketAddr>, tag: u8, state: &SessionState) {
    let bytes = control(tag, &state.id);
    let result = match address {
        Some(addr) => socket.try_send_to(&bytes, addr),
        None => socket.try_send(&bytes),
    };
    if result.is_ok() {
        *state.last_sent.lock().unwrap() = Clock::now();
    }
}

fn heartbeat_delay(options: UdpOptions, random: &mut u64) -> Duration {
    // Timing jitter only; handshake secrets always come from the OS RNG.
    *random ^= *random << 13;
    *random ^= *random >> 7;
    *random ^= *random << 17;
    options.heartbeat_interval
        + options
            .heartbeat_jitter
            .mul_f64((*random >> 11) as f64 / (1u64 << 53) as f64)
}

fn spawn_keepalive(socket: Arc<UdpSocket>, address: Option<SocketAddr>, state: Arc<SessionState>) {
    tokio::spawn(async move {
        let mut random = u64::from_le_bytes(state.id[..8].try_into().unwrap()) | 1;
        loop {
            let delay = heartbeat_delay(state.options, &mut random);
            tokio::select! {
                biased;
                _ = state.closed.cancelled() => break,
                _ = tokio::time::sleep(delay) => {
                    if state.last_sent.lock().unwrap().elapsed() < state.options.heartbeat_interval { continue; }
                    if address.is_some() && (!state.seen.swap(false, Ordering::Relaxed) || !state.take_credit()) { continue; }
                    try_control(&socket, address, KEEPALIVE, &state);
                }
            }
        }
        for _ in 0..DISCONNECT_REPEATS {
            if address.is_some() && !state.take_credit() {
                break;
            }
            try_control(&socket, address, DISCONNECT, &state);
        }
    });
}

/// Accepted UDP session.
pub struct UdpServerStream {
    registration: PeerRegistration,
    incoming: mpsc::Receiver<Queued>,
    state: Arc<SessionState>,
    peer_addr: SocketAddr,
    socket: Arc<UdpSocket>,
}
#[async_trait]
impl NetworkStream for UdpServerStream {
    type ReadHalf = UdpServerReadHalf;
    type WriteHalf = UdpServerWriteHalf;
    async fn into_split(self) -> io::Result<(Self::ReadHalf, Self::WriteHalf)> {
        let incoming = Incoming::Queue {
            queue: self.incoming,
            current: Box::default(),
            _registration: self.registration,
        };
        Ok(split(
            incoming,
            self.socket,
            Some(self.peer_addr),
            self.state,
        ))
    }
    fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }
    fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr().unwrap()
    }
}
impl ServerStream for UdpServerStream {}

/// Connected UDP session.
pub struct ConfiguredUdpClientStream<C: UdpConfig> {
    _config: PhantomData<C>,
    socket: Arc<UdpSocket>,
    state: Arc<SessionState>,
}
/// Client stream with default UDP settings.
pub type UdpClientStream = ConfiguredUdpClientStream<DefaultUdpConfig>;

impl<C: UdpConfig> ConfiguredUdpClientStream<C> {
    async fn connect_with_options(address: SocketAddr, options: UdpOptions) -> io::Result<Self> {
        let options = options.validate()?;
        let local: SocketAddr = match address {
            SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
            SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
        };
        let socket = Arc::new(UdpSocket::bind(local).await?);
        socket.connect(address).await?;
        let cookie = tokio::time::timeout(CONNECT_TIMEOUT, handshake(&socket))
            .await
            .map_err(|_| io::Error::new(ErrorKind::TimedOut, "UDP handshake timed out"))??;
        Ok(Self {
            _config: PhantomData,
            socket,
            state: SessionState::new(cookie, options),
        })
    }
}
#[async_trait]
impl<C: UdpConfig> ClientStream for ConfiguredUdpClientStream<C> {
    async fn connect(addr: SocketAddr) -> io::Result<Self> {
        Self::connect_with_options(addr, C::OPTIONS).await
    }
}
#[async_trait]
impl<C: UdpConfig> NetworkStream for ConfiguredUdpClientStream<C> {
    type ReadHalf = UdpClientReadHalf;
    type WriteHalf = UdpClientWriteHalf;
    async fn into_split(self) -> io::Result<(Self::ReadHalf, Self::WriteHalf)> {
        let incoming = Incoming::Socket {
            socket: Arc::clone(&self.socket),
            buffer: vec![0; BUFFER_SIZE].into_boxed_slice(),
        };
        Ok(split(incoming, self.socket, None, self.state))
    }
    fn peer_addr(&self) -> SocketAddr {
        self.socket.peer_addr().unwrap()
    }
    fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr().unwrap()
    }
}

async fn handshake(socket: &UdpSocket) -> io::Result<Cookie> {
    let nonce = random()?;
    let hello = Cookie::hello(nonce).encode(HELLO);
    let mut selected: Option<Cookie> = None;
    let mut buffer = vec![0; BUFFER_SIZE];
    let mut retry = tokio::time::interval(PROBE_INTERVAL);
    retry.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = retry.tick() => {
                let request = selected.map(|cookie| cookie.encode(CONFIRM)).unwrap_or(hello);
                socket.send(&request).await?;
            }
            result = socket.peek(&mut buffer) => {
                let len = result?;
                if let Some((tag, cookie)) = Cookie::parse(&buffer[..len]) {
                    socket.recv(&mut buffer).await?;
                    if tag == CHALLENGE && cookie.nonce == nonce && selected.is_none_or(|old| cookie.generation > old.generation) {
                        selected = Some(cookie);
                        socket.send(&cookie.encode(CONFIRM)).await?;
                    }
                    continue;
                }
                if let (Some(cookie), Some((tag, session, _))) = (selected, parse_frame(&buffer[..len])) {
                    if session == cookie.mac {
                        match tag {
                            ACCEPT => { socket.recv(&mut buffer).await?; return Ok(cookie); }
                            DATA => return Ok(cookie), // Preserve early application data for the read half.
                            DISCONNECT => return Err(io::Error::new(ErrorKind::ConnectionRefused, "UDP connection refused")),
                            _ => {},
                        }
                    }
                }
                socket.recv(&mut buffer).await?;
            }
        }
    }
}

fn split(
    incoming: Incoming,
    socket: Arc<UdpSocket>,
    address: Option<SocketAddr>,
    state: Arc<SessionState>,
) -> (UdpReadHalf, UdpWriteHalf) {
    spawn_keepalive(Arc::clone(&socket), address, Arc::clone(&state));
    (
        UdpReadHalf {
            incoming,
            state: Arc::clone(&state),
            idle_timeout: watch::channel(UdpIdleTimeout::default().0).1,
            timeout_updates_open: true,
        },
        UdpWriteHalf {
            socket,
            address,
            state,
        },
    )
}

/// Read half of a UDP session; dropping it closes the session and stops heartbeats.
pub struct UdpReadHalf {
    incoming: Incoming,
    state: Arc<SessionState>,
    idle_timeout: watch::Receiver<Duration>,
    timeout_updates_open: bool,
}
/// Server read half.
pub type UdpServerReadHalf = UdpReadHalf;
/// Client read half.
pub type UdpClientReadHalf = UdpReadHalf;
impl Drop for UdpReadHalf {
    fn drop(&mut self) {
        self.state.closed.cancel();
    }
}

#[async_trait]
impl ReadStream for UdpReadHalf {
    fn close(&mut self) {
        self.state.closed.cancel();
    }

    fn set_idle_timeout(&mut self, timeout: watch::Receiver<Duration>) {
        self.idle_timeout = timeout;
        self.timeout_updates_open = true;
    }
    async fn read_exact(&mut self, _: &mut [u8]) -> io::Result<()> {
        Err(io::Error::new(
            ErrorKind::Unsupported,
            "use ReadStream::receive for UDP",
        ))
    }
    async fn receive<R, S, Ser, LS>(
        &mut self,
        serializer: Arc<Ser>,
        length: &LS,
    ) -> Result<R, ReceiveError<Ser::DecodeError, LS>>
    where
        R: Send + Sync + Debug + 'static,
        S: Send + Sync + Debug + 'static,
        Ser: Serializer<R, S> + ?Sized,
        LS: PacketLengthSerializer,
    {
        self.receive_with_timestamp(serializer, length)
            .await
            .map(|(packet, _)| packet)
    }
    async fn receive_with_timestamp<R, S, Ser, LS>(
        &mut self,
        serializer: Arc<Ser>,
        _: &LS,
    ) -> Result<(R, Instant), ReceiveError<Ser::DecodeError, LS>>
    where
        R: Send + Sync + Debug + 'static,
        S: Send + Sync + Debug + 'static,
        Ser: Serializer<R, S> + ?Sized,
        LS: PacketLengthSerializer,
    {
        loop {
            let timeout = *self.idle_timeout.borrow();
            let last = *self.state.last_received.lock().unwrap();
            let deadline = async {
                match last.checked_add(timeout) {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            };
            let from_socket = matches!(self.incoming, Incoming::Socket { .. });
            let (bytes, received_at) = tokio::select! {
                biased;
                _ = self.state.closed.cancelled() => return Err(ReceiveError::Io(disconnected_error())),
                _ = deadline => {
                    // The listener may have received keepalives while this read waited on its queue.
                    if *self.state.last_received.lock().unwrap() != last { continue; }
                    self.state.closed.cancel();
                    return Err(ReceiveError::Io(io::Error::new(ErrorKind::TimedOut, "no datagrams from the UDP peer")));
                }
                changed = self.idle_timeout.changed(), if self.timeout_updates_open => {
                    self.timeout_updates_open = changed.is_ok(); continue;
                }
                result = self.incoming.next(&self.state) => result.map_err(|err| {
                    self.state.closed.cancel();
                    ReceiveError::Io(err)
                })?,
            };
            let Some((tag, session, payload)) = parse_frame(bytes) else {
                continue;
            };
            if session != self.state.id || tag == ACCEPT {
                continue;
            }
            if from_socket {
                self.state.received();
            }
            if tag == DISCONNECT {
                self.state.closed.cancel();
                return Err(ReceiveError::Io(disconnected_error()));
            }
            if tag != DATA || payload.len() > MAX_PACKET_SIZE.load(Ordering::Relaxed) {
                continue;
            }
            match serializer.deserialize(payload) {
                Ok(packet) => return Ok((packet, received_at)),
                Err(err) => log::debug!("Dropping malformed UDP payload: {err}"),
            }
        }
    }
}

/// Write half of a UDP session.
pub struct UdpWriteHalf {
    socket: Arc<UdpSocket>,
    address: Option<SocketAddr>,
    state: Arc<SessionState>,
}
impl UdpWriteHalf {
    /// Number of application packets dropped because they exceeded the configured datagram size.
    pub fn dropped_oversized_packets(&self) -> usize {
        self.state.dropped_oversized.load(Ordering::Relaxed)
    }
    /// Number of application packets dropped after a socket send error.
    pub fn dropped_send_errors(&self) -> usize {
        self.state.dropped_send_errors.load(Ordering::Relaxed)
    }
}

/// Server write half.
pub type UdpServerWriteHalf = UdpWriteHalf;
/// Client write half.
pub type UdpClientWriteHalf = UdpWriteHalf;
#[async_trait]
impl WriteStream for UdpWriteHalf {
    /// Sends raw wire bytes. Prefer `send`, which adds the session header.
    async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.state.closed.is_cancelled() {
            return Err(disconnected_error());
        }
        let sent = match self.address {
            Some(addr) => self.socket.send_to(bytes, addr).await?,
            None => self.socket.send(bytes).await?,
        };
        if sent != bytes.len() {
            return Err(io::Error::from(ErrorKind::WriteZero));
        }
        *self.state.last_sent.lock().unwrap() = Clock::now();
        Ok(())
    }
    async fn send<R, S, Ser, LS>(
        &mut self,
        packet: S,
        serializer: Arc<Ser>,
        _: &LS,
    ) -> io::Result<()>
    where
        R: Send + Sync + Debug + 'static,
        S: Send + Sync + Debug + 'static,
        Ser: Serializer<R, S> + ?Sized,
        LS: PacketLengthSerializer,
    {
        let payload = serializer
            .serialize(packet)
            .map_err(|err| io::Error::other(err.to_string()))?;
        if payload.len() > self.state.options.max_datagram_size - HEADER {
            self.state.dropped_oversized.fetch_add(1, Ordering::Relaxed);
            log::warn!("Dropping oversized UDP payload ({} bytes)", payload.len());
            return Ok(());
        }
        if let Err(err) = self.write_all(&frame(DATA, &self.state.id, &payload)).await {
            if self.state.closed.is_cancelled() {
                return Err(err);
            }
            self.state
                .dropped_send_errors
                .fetch_add(1, Ordering::Relaxed);
            log::warn!("Dropping UDP packet: {err}");
        }
        Ok(())
    }
}
fn disconnected_error() -> io::Error {
    io::Error::new(ErrorKind::ConnectionAborted, "UDP session closed")
}

#[cfg(test)]
mod tests;

// Raw-peer fixture shared with the ECS integration regressions. Production peers use CookieJar.
#[cfg(test)]
pub(crate) fn test_answer(bytes: &[u8]) -> Option<Vec<u8>> {
    if let Some((tag, cookie)) = Cookie::parse(bytes) {
        return match tag {
            HELLO => Some(
                Cookie {
                    mac: *blake3::hash(&cookie.nonce).as_bytes(),
                    generation: 1,
                    ..cookie
                }
                .encode(CHALLENGE)
                .to_vec(),
            ),
            CONFIRM => Some(control(ACCEPT, &cookie.mac).to_vec()),
            _ => None,
        };
    }
    match parse_frame(bytes) {
        Some((KEEPALIVE, session, _)) => Some(control(KEEPALIVE, &session).to_vec()),
        _ => None,
    }
}
