use super::session::{Peer, PeerRegistration, Peers, SessionState};
use super::settings::{UdpOptions, ValidatedOptions, BUFFER_SIZE, MAX_QUEUED_DATAGRAMS};
use super::stream::UdpServerStream;
use super::wire::{Control, CookieJar, Frame, HandshakeFrame, HandshakeKind, Payload};
use crate::protocol::Listener;
use async_trait::async_trait;
use bevy::platform::time::Instant;
use std::{
    io::{self, ErrorKind},
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    net::UdpSocket,
    sync::{mpsc, Semaphore},
    time::Instant as Clock,
};

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
    options: ValidatedOptions,
    admission: Mutex<Admission>,
    slots: Arc<Semaphore>,
}
impl UdpNetworkListener {
    pub(super) async fn bind(address: SocketAddr, options: UdpOptions) -> io::Result<Self> {
        let options = ValidatedOptions::new(options)?;
        Ok(Self {
            slots: Arc::new(Semaphore::new(options.max_peers())),
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
    pub(super) fn dispatch(
        &self,
        bytes: &[u8],
        address: SocketAddr,
        received_at: Instant,
    ) -> Option<UdpServerStream> {
        if let Some(HandshakeFrame { kind, cookie }) = HandshakeFrame::parse(bytes) {
            if !matches!(kind, HandshakeKind::Hello | HandshakeKind::Confirm)
                || !self
                    .admission
                    .lock()
                    .unwrap()
                    .take(self.options.max_handshake_packets_per_second())
            {
                return None;
            }
            if kind == HandshakeKind::Hello {
                let challenge =
                    HandshakeFrame::challenge(self.cookies.issue(address, cookie.nonce)).encode();
                let _ = self.socket.try_send_to(&challenge, address);
                return None;
            }
            let mut peers = self.peers.lock().unwrap();
            if let Some(peer) = peers.get(&address) {
                if peer.state().id() == cookie.mac {
                    let reply = if peer.state().is_closed() {
                        Control::Disconnect
                    } else {
                        Control::Accept
                    };
                    let _ = self.socket.try_send_to(&reply.encode(cookie.mac), address);
                    return None;
                }
                // A delayed confirmation must not replace a newer session.
                if cookie.generation <= peer.state().generation() {
                    return None;
                }
            }
            if !self.cookies.verify(address, &cookie) {
                return None;
            }
            let Ok(slot) = Arc::clone(&self.slots).try_acquire_owned() else {
                let _ = self
                    .socket
                    .try_send_to(&Control::Disconnect.encode(cookie.mac), address);
                return None;
            };
            let state = SessionState::new(cookie, self.options);
            let (queue, incoming) = mpsc::channel(MAX_QUEUED_DATAGRAMS);
            if let Some(old) = peers.insert(address, Peer::new(queue, Arc::clone(&state))) {
                old.state().close();
            }
            let _ = self
                .socket
                .try_send_to(&Control::Accept.encode(state.id()), address);
            return Some(UdpServerStream::accepted(
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
            ));
        }
        if let Some(Frame { session, payload }) = Frame::parse(bytes) {
            if payload == Payload::Control(Control::Accept) {
                return None;
            }
            let peers = self.peers.lock().unwrap();
            match peers
                .get(&address)
                .filter(|peer| peer.state().id() == session)
            {
                Some(peer) => {
                    peer.state().received();
                    match payload {
                        Payload::Control(Control::Disconnect) => peer.state().close(),
                        Payload::Data(_) => peer.push(bytes, received_at),
                        _ => {}
                    }
                }
                None if payload != Payload::Control(Control::Disconnect) => {
                    let _ = self
                        .socket
                        .try_send_to(&Control::Disconnect.encode(session), address);
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

#[cfg(test)]
impl UdpNetworkListener {
    pub(super) fn peer_count(&self) -> usize {
        self.peers.lock().unwrap().len()
    }
    pub(super) fn issue_cookie(
        &self,
        address: SocketAddr,
        nonce: super::wire::Nonce,
    ) -> super::wire::Cookie {
        self.cookies.issue(address, nonce)
    }
}
