use super::peer::{Peer, PeerRegistration, PeerState, Peers, QueuedDatagram};
use super::settings::{UdpOptions, ValidatedOptions, BUFFER_SIZE};
use super::stream::UdpServerStream;
use crate::{packet_queue::lossy_channel, protocols::protocol::Listener};
use async_trait::async_trait;
use bevy::platform::time::Instant;
use std::{
    io::{self, ErrorKind},
    net::SocketAddr,
    sync::Arc,
};
use tokio::{net::UdpSocket, sync::Semaphore};

/// Routes full datagrams to local peers by source address. No sender validation is performed.
pub struct UdpNetworkListener {
    socket: Arc<UdpSocket>,
    local_addr: SocketAddr,
    peers: Arc<Peers>,
    options: ValidatedOptions,
    slots: Arc<Semaphore>,
}
impl UdpNetworkListener {
    pub(super) async fn bind(address: SocketAddr, options: UdpOptions) -> io::Result<Self> {
        let options = ValidatedOptions::new(options)?;
        let socket = Arc::new(UdpSocket::bind(address).await?);
        Ok(Self {
            local_addr: socket.local_addr()?,
            socket,
            peers: Arc::default(),
            slots: Arc::new(Semaphore::new(options.max_peers())),
            options,
        })
    }
    pub(super) fn dispatch(
        &self,
        bytes: &[u8],
        address: SocketAddr,
        received_at: Instant,
    ) -> Option<UdpServerStream> {
        if bytes.len() > super::MAX_DATAGRAM_SIZE {
            return None;
        }
        let mut peers = self
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(peer) = peers.get(&address).filter(|peer| !peer.state().is_closed()) {
            peer.state().received_datagram(bytes.len());
            peer.push(bytes, received_at);
            return None;
        }
        if self.options.receive_queue_capacity() == 0
            || bytes.len() > self.options.receive_queue_bytes()
        {
            return None;
        }
        let slot = Arc::clone(&self.slots).try_acquire_owned().ok()?;
        let state = PeerState::new(self.options);
        let (queue, incoming) = lossy_channel(
            self.options.receive_queue_capacity(),
            self.options.receive_queue_bytes(),
            self.options.receive_queue_overflow(),
        );
        queue
            .try_send(
                QueuedDatagram {
                    bytes: bytes.into(),
                    received_at,
                },
                bytes.len(),
            )
            .ok()?;
        state.received_datagram(bytes.len());
        peers.insert(address, Peer::new(queue, Arc::clone(&state)));
        drop(peers);
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
}
impl Drop for UdpNetworkListener {
    fn drop(&mut self) {
        let mut peers = self
            .peers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for peer in peers.values() {
            peer.state().close();
        }
        peers.clear();
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
