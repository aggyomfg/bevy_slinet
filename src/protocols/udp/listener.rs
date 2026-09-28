use super::diagnostics::UdpDropReason;
use super::session::{Peer, PeerRegistration, Peers, SessionState};
use super::settings::{UdpOptions, ValidatedOptions, BUFFER_SIZE};
use super::stream::UdpServerStream;
use super::wire::{Control, Cookie, CookieJar, Frame, HandshakeFrame, HandshakeKind, Payload};
use crate::{packet_queue::lossy_channel, protocols::protocol::Listener};
use async_trait::async_trait;
use bevy::platform::time::Instant;
use std::{
    collections::HashMap,
    io::{self, ErrorKind},
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::UdpSocket, sync::Semaphore, time::Instant as Clock};

struct HandshakeAdmission {
    packets_per_second: usize,
    started: Clock,
    used: usize,
}
impl HandshakeAdmission {
    fn new(packets_per_second: usize) -> Self {
        Self {
            packets_per_second,
            started: Clock::now(),
            used: 0,
        }
    }

    fn take(&mut self) -> bool {
        if self.started.elapsed() >= Duration::from_secs(1) {
            self.started = Clock::now();
            self.used = 0;
        }
        if self.used >= self.packets_per_second {
            return false;
        }
        self.used += 1;
        true
    }
}

#[derive(Clone, Copy)]
struct ReplayWatermark {
    generation: u64,
    epoch: u64,
}

#[derive(Default)]
struct ReplayWatermarks(HashMap<SocketAddr, ReplayWatermark>);
impl ReplayWatermarks {
    fn allows(
        &mut self,
        address: SocketAddr,
        cookie: super::wire::Cookie,
        jar: &CookieJar,
        capacity: usize,
    ) -> bool {
        if let Some(previous) = self.0.get(&address).copied() {
            if jar.epoch_is_live(previous.epoch) {
                return cookie.generation > previous.generation;
            }
            self.0.remove(&address);
        }
        if self.0.len() >= capacity {
            self.0.retain(|_, entry| jar.epoch_is_live(entry.epoch));
        }
        self.0.len() < capacity
    }

    fn record(&mut self, address: SocketAddr, cookie: super::wire::Cookie) {
        self.0.insert(
            address,
            ReplayWatermark {
                generation: cookie.generation,
                epoch: cookie.epoch,
            },
        );
    }
}

/// A shared UDP socket that routes packets to validated sessions.
pub struct UdpNetworkListener {
    socket: Arc<UdpSocket>,
    local_addr: SocketAddr,
    peers: Arc<Peers>,
    cookies: CookieJar,
    options: ValidatedOptions,
    admission: Mutex<HandshakeAdmission>,
    replay: Mutex<ReplayWatermarks>,
    slots: Arc<Semaphore>,
}
impl UdpNetworkListener {
    fn send_session_control(&self, state: &SessionState, control: Control, address: SocketAddr) {
        let bytes = control.encode(state.id());
        match self.socket.try_send_to(&bytes, address) {
            Ok(_) => {
                state.sent();
                state.handle().control_sent(bytes.len());
            }
            Err(_) => state.drop_send_error(),
        }
    }

    pub(super) async fn bind(address: SocketAddr, options: UdpOptions) -> io::Result<Self> {
        let options = ValidatedOptions::new(options)?;
        let socket = Arc::new(UdpSocket::bind(address).await?);
        let local_addr = socket.local_addr()?;
        Ok(Self {
            slots: Arc::new(Semaphore::new(options.max_peers())),
            socket,
            local_addr,
            peers: Arc::default(),
            cookies: CookieJar::new()?,
            replay: Mutex::new(ReplayWatermarks::default()),
            options,
            admission: Mutex::new(HandshakeAdmission::new(
                options.max_handshake_packets_per_second(),
            )),
        })
    }

    fn dispatch_handshake(
        &self,
        kind: HandshakeKind,
        cookie: Cookie,
        address: SocketAddr,
    ) -> Option<UdpServerStream> {
        if !matches!(kind, HandshakeKind::Hello | HandshakeKind::Confirm)
            || !self
                .admission
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
        {
            return None;
        }
        if kind == HandshakeKind::Hello {
            let challenge =
                HandshakeFrame::challenge(self.cookies.issue(address, cookie.nonce)).encode();
            let _ = self.socket.try_send_to(&challenge, address);
            return None;
        }
        let mut peers = self
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(peer) = peers.get(&address) {
            if peer.state().id() == cookie.mac {
                let reply = if peer.state().is_closed() {
                    Control::Disconnect
                } else {
                    Control::Accept
                };
                self.send_session_control(peer.state(), reply, address);
                return None;
            }
            if !peer.state().can_be_replaced_by(cookie) {
                return None;
            }
        }
        if !self.cookies.verify(address, &cookie) {
            return None;
        }
        let mut replay = self
            .replay
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !replay.allows(
            address,
            cookie,
            &self.cookies,
            self.options.max_replay_entries(),
        ) {
            return None;
        }
        let Ok(slot) = Arc::clone(&self.slots).try_acquire_owned() else {
            let _ = self
                .socket
                .try_send_to(&Control::Disconnect.encode(cookie.mac), address);
            return None;
        };
        // The peer lock serializes slot acquisition and watermark advancement.
        // A failed replacement leaves the previous watermark untouched.
        replay.record(address, cookie);
        drop(replay);
        let state = SessionState::new(cookie, self.options);
        let (queue, incoming) = lossy_channel(
            self.options.receive_queue_capacity(),
            self.options.receive_queue_bytes(),
            self.options.receive_queue_overflow(),
        );
        if let Some(old) = peers.insert(address, Peer::new(queue, Arc::clone(&state))) {
            old.state().close();
        }
        drop(peers);
        self.send_session_control(&state, Control::Accept, address);
        Some(UdpServerStream::accepted(
            PeerRegistration::new(
                slot,
                Arc::downgrade(&self.peers),
                address,
                Arc::clone(&state),
            ),
            incoming,
            state,
            address,
            Arc::clone(&self.socket),
            self.local_addr,
        ))
    }

    // Synchronous dispatch keeps all control sends out of the socket receive wait path.
    pub(super) fn dispatch(
        &self,
        bytes: &[u8],
        address: SocketAddr,
        received_at: Instant,
    ) -> Option<UdpServerStream> {
        if let Some(HandshakeFrame { kind, cookie }) = HandshakeFrame::parse(bytes) {
            return self.dispatch_handshake(kind, cookie, address);
        }
        if let Some(Frame { session, payload }) = Frame::parse(bytes) {
            let peers = self
                .peers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match peers
                .get(&address)
                .filter(|peer| peer.state().id() == session)
            {
                Some(peer) => {
                    peer.state()
                        .received_datagram(bytes.len(), matches!(payload, Payload::Data(_)));
                    match payload {
                        Payload::Control(Control::Disconnect) => peer.state().close(),
                        Payload::Data(_) => peer.push(bytes, received_at),
                        Payload::Control(Control::Keepalive | Control::Accept) => {}
                    }
                }
                None if !matches!(
                    payload,
                    Payload::Control(Control::Disconnect | Control::Accept)
                ) =>
                {
                    let _ = self
                        .socket
                        .try_send_to(&Control::Disconnect.encode(session), address);
                }
                None => {}
            }
        } else if let Some(peer) = self
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&address)
        {
            peer.state()
                .handle()
                .count_drop(UdpDropReason::MalformedPayload);
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
            let bytes = buffer
                .get(..len)
                .ok_or_else(|| io::Error::from(ErrorKind::InvalidData))?;
            if let Some(stream) = self.dispatch(bytes, address, Instant::now()) {
                return Ok(stream);
            }
        }
    }
    fn address(&self) -> SocketAddr {
        self.local_addr
    }
}

#[cfg(test)]
impl UdpNetworkListener {
    pub(super) fn peer_count(&self) -> usize {
        self.peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
    pub(super) fn issue_cookie(
        &self,
        address: SocketAddr,
        nonce: super::wire::Nonce,
    ) -> super::wire::Cookie {
        self.cookies.issue(address, nonce)
    }
}
