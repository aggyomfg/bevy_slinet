use super::diagnostics::{UdpConnectionHandle, UdpDropReason};
use super::settings::ValidatedOptions;
use crate::connection::LossySender;
use bevy::platform::time::Instant;
use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::{mpsc::error::TrySendError, OwnedSemaphorePermit};
use tokio_util::sync::CancellationToken;

pub(super) struct QueuedDatagram {
    pub bytes: Box<[u8]>,
    pub received_at: Instant,
}
pub(super) type Peers = Mutex<HashMap<SocketAddr, Peer>>;

pub(super) struct PeerState {
    options: ValidatedOptions,
    pub(super) handle: UdpConnectionHandle,
    closed: CancellationToken,
}
impl PeerState {
    pub(super) fn new(options: ValidatedOptions) -> Arc<Self> {
        Arc::new(Self {
            options,
            handle: UdpConnectionHandle::new(options.max_payload_size(), options.send_rate()),
            closed: CancellationToken::new(),
        })
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
    pub(super) fn received_datagram(&self, bytes: usize) {
        self.handle.received(bytes);
    }
    pub(super) fn disconnected_error() -> io::Error {
        io::Error::new(io::ErrorKind::ConnectionAborted, "UDP peer closed")
    }
}

pub(super) struct Peer {
    queue: LossySender<QueuedDatagram>,
    state: Arc<PeerState>,
}
impl Peer {
    pub(super) const fn new(queue: LossySender<QueuedDatagram>, state: Arc<PeerState>) -> Self {
        Self { queue, state }
    }
    pub(super) fn state(&self) -> &PeerState {
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
    state: Arc<PeerState>,
}
impl PeerRegistration {
    pub(super) const fn new(
        slot: OwnedSemaphorePermit,
        peers: Weak<Peers>,
        address: SocketAddr,
        state: Arc<PeerState>,
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
