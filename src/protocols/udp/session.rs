use super::settings::{ValidatedOptions, DISCONNECT_REPEATS, MAX_QUEUED_BYTES};
use super::wire::{Control, Cookie, Session};
use bevy::platform::time::Instant;
use std::{
    collections::HashMap,
    io::{self, ErrorKind},
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};
use tokio::{
    net::UdpSocket,
    sync::{mpsc, OwnedSemaphorePermit},
    time::Instant as Clock,
};
use tokio_util::sync::CancellationToken;

pub(super) struct QueuedDatagram {
    pub bytes: Box<[u8]>,
    pub received_at: Instant,
}
pub(super) type Peers = Mutex<HashMap<SocketAddr, Peer>>;

#[derive(Default)]
struct ResponseBudget {
    received_since_heartbeat: AtomicBool,
    credits: AtomicUsize,
}
impl ResponseBudget {
    const CAPACITY: usize = DISCONNECT_REPEATS + 1;

    fn received(&self) {
        self.received_since_heartbeat.store(true, Ordering::Relaxed);
        let _ = self
            .credits
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |credit| {
                Some((credit + 1).min(Self::CAPACITY))
            });
    }

    fn take(&self) -> bool {
        self.credits
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |credit| {
                credit.checked_sub(1)
            })
            .is_ok()
    }

    fn take_heartbeat(&self) -> bool {
        self.received_since_heartbeat.swap(false, Ordering::Relaxed) && self.take()
    }
}

pub(super) struct SessionState {
    id: Session,
    generation: u64,
    options: ValidatedOptions,
    queued_bytes: AtomicUsize,
    responses: ResponseBudget,
    last_received: Mutex<Clock>,
    last_sent: Mutex<Clock>,
    dropped_oversized: AtomicUsize,
    dropped_send_errors: AtomicUsize,
    closed: CancellationToken,
}
impl SessionState {
    pub(super) fn id(&self) -> Session {
        self.id
    }
    pub(super) fn can_be_replaced_by(&self, cookie: Cookie) -> bool {
        cookie.generation > self.generation
    }
    pub(super) fn options(&self) -> ValidatedOptions {
        self.options
    }
    pub(super) fn close(&self) {
        self.closed.cancel();
    }
    pub(super) fn is_closed(&self) -> bool {
        self.closed.is_cancelled()
    }
    pub(super) async fn cancelled(&self) {
        self.closed.cancelled().await;
    }
    pub(super) fn last_received(&self) -> Clock {
        *self.last_received.lock().unwrap()
    }
    pub(super) fn sent(&self) {
        *self.last_sent.lock().unwrap() = Clock::now();
    }
    pub(super) fn dequeue(&self, bytes: usize) {
        self.queued_bytes.fetch_sub(bytes, Ordering::Relaxed);
    }
    pub(super) fn drop_oversized(&self) {
        self.dropped_oversized.fetch_add(1, Ordering::Relaxed);
    }
    pub(super) fn drop_send_error(&self) {
        self.dropped_send_errors.fetch_add(1, Ordering::Relaxed);
    }
    pub(super) fn dropped_oversized(&self) -> usize {
        self.dropped_oversized.load(Ordering::Relaxed)
    }
    pub(super) fn dropped_send_errors(&self) -> usize {
        self.dropped_send_errors.load(Ordering::Relaxed)
    }
    #[cfg(test)]
    pub(super) fn queued_bytes(&self) -> usize {
        self.queued_bytes.load(Ordering::Relaxed)
    }
    pub(super) fn new(cookie: Cookie, options: ValidatedOptions) -> Arc<Self> {
        Arc::new(Self {
            id: cookie.mac,
            generation: cookie.generation,
            options,
            queued_bytes: AtomicUsize::new(0),
            responses: ResponseBudget::default(),
            last_received: Mutex::new(Clock::now()),
            last_sent: Mutex::new(Clock::now()),
            dropped_oversized: AtomicUsize::new(0),
            dropped_send_errors: AtomicUsize::new(0),
            closed: CancellationToken::new(),
        })
    }
    pub(super) fn received(&self) {
        *self.last_received.lock().unwrap() = Clock::now();
        self.responses.received();
    }
    fn heartbeat_due(&self) -> bool {
        self.last_sent.lock().unwrap().elapsed() >= self.options.heartbeat_interval()
    }
}

pub(super) struct Peer {
    queue: mpsc::Sender<QueuedDatagram>,
    state: Arc<SessionState>,
}
impl Peer {
    pub(super) fn new(queue: mpsc::Sender<QueuedDatagram>, state: Arc<SessionState>) -> Self {
        Self { queue, state }
    }
    pub(super) fn state(&self) -> &SessionState {
        &self.state
    }

    pub(super) fn push(&self, bytes: &[u8], received_at: Instant) {
        if self.state.queued_bytes.load(Ordering::Relaxed) + bytes.len() > MAX_QUEUED_BYTES {
            return;
        }
        if let Ok(permit) = self.queue.try_reserve() {
            self.state
                .queued_bytes
                .fetch_add(bytes.len(), Ordering::Relaxed);
            permit.send(QueuedDatagram {
                bytes: bytes.into(),
                received_at,
            });
        }
    }
}

pub(super) struct PeerRegistration {
    _slot: OwnedSemaphorePermit,
    peers: Weak<Peers>,
    address: SocketAddr,
    state: Arc<SessionState>,
}
impl PeerRegistration {
    pub(super) fn new(
        slot: OwnedSemaphorePermit,
        peers: Weak<Peers>,
        address: SocketAddr,
        state: Arc<SessionState>,
    ) -> Self {
        Self {
            _slot: slot,
            peers,
            address,
            state,
        }
    }
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

pub(super) struct Heartbeat {
    socket: Arc<UdpSocket>,
    address: Option<SocketAddr>,
    state: Arc<SessionState>,
}
impl Heartbeat {
    pub(super) fn new(
        socket: Arc<UdpSocket>,
        address: Option<SocketAddr>,
        state: Arc<SessionState>,
    ) -> Self {
        Self {
            socket,
            address,
            state,
        }
    }
    fn send_control(&self, control: Control) {
        let bytes = control.encode(self.state.id);
        let result = match self.address {
            Some(addr) => self.socket.try_send_to(&bytes, addr),
            None => self.socket.try_send(&bytes),
        };
        if result.is_ok() {
            self.state.sent();
        }
    }

    pub(super) fn spawn(self) {
        tokio::spawn(self.run());
    }

    async fn run(self) {
        let state = &self.state;
        let mut timing = HeartbeatTiming::new(
            state.options,
            u64::from_le_bytes(state.id.as_bytes()[..8].try_into().unwrap()),
        );
        loop {
            let delay = timing.next_delay();
            tokio::select! {
                biased;
                _ = state.cancelled() => break,
                _ = tokio::time::sleep(delay) => {
                    if !state.heartbeat_due() { continue; }
                    if self.address.is_some() && !state.responses.take_heartbeat() { continue; }
                    self.send_control(Control::Keepalive);
                }
            }
        }
        for _ in 0..DISCONNECT_REPEATS {
            if self.address.is_some() && !state.responses.take() {
                break;
            }
            self.send_control(Control::Disconnect);
        }
    }
}
/// Non-cryptographic timing jitter; handshake secrets use the OS RNG.
pub(super) struct HeartbeatTiming {
    options: ValidatedOptions,
    random: u64,
}
impl HeartbeatTiming {
    pub(super) fn new(options: ValidatedOptions, seed: u64) -> Self {
        Self {
            options,
            random: seed | 1,
        }
    }
    pub(super) fn next_delay(&mut self) -> Duration {
        self.random ^= self.random << 13;
        self.random ^= self.random >> 7;
        self.random ^= self.random << 17;
        self.options.heartbeat_interval()
            + self
                .options
                .heartbeat_jitter()
                .mul_f64((self.random >> 11) as f64 / (1u64 << 53) as f64)
    }
}

impl SessionState {
    pub(super) fn disconnected_error() -> io::Error {
        io::Error::new(ErrorKind::ConnectionAborted, "UDP session closed")
    }
}
