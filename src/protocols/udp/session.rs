use super::diagnostics::{UdpConnectionHandle, UdpDropReason};
use super::settings::{ValidatedOptions, DISCONNECT_REPEATS};
use super::wire::{Control, Cookie, Session};
use crate::packet_queue::LossySender;
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
    sync::{mpsc::error::TrySendError, OwnedSemaphorePermit},
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
    pub(super) handle: UdpConnectionHandle,
    responses: ResponseBudget,
    last_received: Mutex<Clock>,
    last_sent: Mutex<Clock>,
    closed: CancellationToken,
}
impl SessionState {
    pub(super) const fn id(&self) -> Session {
        self.id
    }
    pub(super) const fn can_be_replaced_by(&self, cookie: Cookie) -> bool {
        cookie.generation > self.generation
    }
    pub(super) const fn options(&self) -> ValidatedOptions {
        self.options
    }
    pub(super) fn handle(&self) -> UdpConnectionHandle {
        self.handle.clone()
    }
    pub(super) const fn closed_token(&self) -> &CancellationToken {
        &self.closed
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
        *self
            .last_received
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    pub(super) fn sent(&self) {
        *self
            .last_sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Clock::now();
    }
    pub(super) fn drop_oversized(&self) {
        self.handle.count_drop(UdpDropReason::OversizedPayload);
    }
    pub(super) fn drop_send_error(&self) {
        self.handle.count_drop(UdpDropReason::SocketSendError);
    }
    pub(super) fn dropped_oversized(&self) -> usize {
        usize::try_from(self.handle.stats().dropped_oversized_payload).unwrap_or(usize::MAX)
    }
    pub(super) fn dropped_send_errors(&self) -> usize {
        usize::try_from(self.handle.stats().dropped_socket_send_error).unwrap_or(usize::MAX)
    }
    pub(super) fn new(cookie: Cookie, options: ValidatedOptions) -> Arc<Self> {
        Arc::new(Self {
            id: cookie.mac,
            generation: cookie.generation,
            options,
            handle: UdpConnectionHandle::new(options.max_payload_size(), options.send_rate()),
            responses: ResponseBudget::default(),
            last_received: Mutex::new(Clock::now()),
            last_sent: Mutex::new(Clock::now()),
            closed: CancellationToken::new(),
        })
    }
    pub(super) fn received(&self) {
        *self
            .last_received
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Clock::now();
        self.responses.received();
    }
    pub(super) fn received_datagram(&self, bytes: usize, data: bool) {
        self.received();
        self.handle.received(bytes, data);
    }
    fn heartbeat_due(&self) -> bool {
        self.last_sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .elapsed()
            >= self.options.heartbeat_interval()
    }
}

pub(super) struct Peer {
    queue: LossySender<QueuedDatagram>,
    state: Arc<SessionState>,
}
impl Peer {
    pub(super) const fn new(queue: LossySender<QueuedDatagram>, state: Arc<SessionState>) -> Self {
        Self { queue, state }
    }
    pub(super) fn state(&self) -> &SessionState {
        &self.state
    }

    pub(super) fn push(&self, bytes: &[u8], received_at: Instant) {
        if self.state.is_closed() {
            self.state
                .handle
                .count_drop(UdpDropReason::ClosedBeforeDelivery);
            return;
        }
        match self.queue.try_send(
            QueuedDatagram {
                bytes: bytes.into(),
                received_at,
            },
            bytes.len(),
        ) {
            Ok(evicted) => {
                for _ in evicted {
                    self.state.handle.count_drop(UdpDropReason::RawQueueEvicted);
                }
            }
            Err(TrySendError::Full(_)) => self.state.handle.count_drop(UdpDropReason::RawQueueFull),
            Err(TrySendError::Closed(_)) => self
                .state
                .handle
                .count_drop(UdpDropReason::ClosedBeforeDelivery),
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
    pub(super) const fn new(
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
            let mut peers = peers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    pub(super) const fn new(
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
    async fn send_control(&self, control: Control) {
        let bytes = control.encode(self.state.id);
        let result = tokio::time::timeout(Duration::from_millis(250), async {
            match self.address {
                Some(addr) => self.socket.send_to(&bytes, addr).await,
                None => self.socket.send(&bytes).await,
            }
        })
        .await;
        if matches!(result, Ok(Ok(sent)) if sent == bytes.len()) {
            self.state.sent();
            self.state.handle.control_sent(bytes.len());
        } else {
            self.state.drop_send_error();
        }
    }

    pub(super) fn spawn(self) {
        tokio::spawn(self.run());
    }

    async fn run(self) {
        let state = &self.state;
        let (seed, _) = state.id.as_bytes().split_at(8);
        let mut seed_bytes = [0; 8];
        seed_bytes.copy_from_slice(seed);
        let mut timing = HeartbeatTiming::new(state.options, u64::from_le_bytes(seed_bytes));
        loop {
            let delay = timing.next_delay();
            tokio::select! {
                biased;
                () = state.cancelled() => break,
                () = tokio::time::sleep(delay) => {
                    if !state.heartbeat_due() { continue; }
                    if self.address.is_some() && !state.responses.take_heartbeat() { continue; }
                    tokio::select! {
                        biased;
                        () = state.cancelled() => break,
                        () = self.send_control(Control::Keepalive) => {}
                    }
                }
            }
        }
        let disconnect_started = Clock::now();
        for attempt in 0..DISCONNECT_REPEATS {
            tokio::time::sleep_until(disconnect_started + Duration::from_secs(attempt as u64))
                .await;
            if self.address.is_some() && !state.responses.take() {
                break;
            }
            self.send_control(Control::Disconnect).await;
        }
    }
}
/// Non-cryptographic timing jitter; handshake secrets use the OS RNG.
pub(super) struct HeartbeatTiming {
    options: ValidatedOptions,
    random: u64,
}
impl HeartbeatTiming {
    pub(super) const fn new(options: ValidatedOptions, seed: u64) -> Self {
        Self {
            options,
            random: seed | 1,
        }
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "The shifted random value has at most 53 bits and the divisor is an exact power of two"
    )]
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
