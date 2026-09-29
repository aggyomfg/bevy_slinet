use super::diagnostics::{UdpConnectionHandle, UdpDropReason};
use super::pacing::DataPacer;
use super::peer::{PeerRegistration, PeerState, QueuedDatagram};
use super::settings::{DefaultUdpConfig, UdpConfig, UdpOptions, ValidatedOptions, BUFFER_SIZE};
use crate::{
    connection::ReceiveLimits,
    packet_queue::LossyReceiver,
    protocols::protocol::{
        ClientStream, NetworkStream, PacketReader, PacketWriter, ReceiveError, ServerStream,
    },
    serializers::serializer::Serializer,
    PacketLengthSerializer,
};
use async_trait::async_trait;
use bevy::{log, platform::time::Instant};
use std::{
    fmt::Debug,
    io::{self, ErrorKind},
    marker::PhantomData,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::Arc,
};
use tokio::net::UdpSocket;

struct TrackedIncoming {
    queue: LossyReceiver<QueuedDatagram>,
    state: Arc<PeerState>,
}
impl TrackedIncoming {
    const fn new(queue: LossyReceiver<QueuedDatagram>, state: Arc<PeerState>) -> Self {
        Self { queue, state }
    }
    async fn recv(&mut self) -> Option<QueuedDatagram> {
        self.queue.recv().await
    }
    #[cfg(test)]
    fn queued_bytes(&self) -> usize {
        self.queue.queued_bytes()
    }
}
impl Drop for TrackedIncoming {
    fn drop(&mut self) {
        for _ in self.queue.close_and_drain() {
            self.state
                .handle
                .count_drop(UdpDropReason::ClosedBeforeDelivery);
        }
    }
}

enum Incoming {
    Queue {
        queue: TrackedIncoming,
        current: Box<[u8]>,
        _registration: PeerRegistration,
    },
    Socket {
        socket: Arc<UdpSocket>,
        buffer: Box<[u8]>,
    },
}
struct ReceivedDatagram<'a> {
    bytes: &'a [u8],
    received_at: Instant,
}
impl Incoming {
    async fn next(&mut self) -> io::Result<ReceivedDatagram<'_>> {
        match self {
            Self::Queue { queue, current, .. } => {
                let QueuedDatagram { bytes, received_at } = queue
                    .recv()
                    .await
                    .ok_or_else(PeerState::disconnected_error)?;
                *current = bytes;
                Ok(ReceivedDatagram {
                    bytes: current,
                    received_at,
                })
            }
            Self::Socket { socket, buffer } => {
                let len = loop {
                    match socket.recv(buffer).await {
                        Ok(len) => break len,
                        Err(err)
                            if matches!(
                                err.kind(),
                                ErrorKind::ConnectionReset | ErrorKind::ConnectionRefused
                            ) =>
                        {
                            // A previous datagram can provoke ICMP without closing this socket.
                            // Yield so the caller can observe cancellation before another retry.
                            tokio::task::yield_now().await;
                        }
                        Err(err) => return Err(err),
                    }
                };
                Ok(ReceivedDatagram {
                    bytes: buffer
                        .get(..len)
                        .ok_or_else(|| io::Error::from(ErrorKind::InvalidData))?,
                    received_at: Instant::now(),
                })
            }
        }
    }
}

/// Local server-side UDP address association.
pub struct UdpServerStream {
    registration: PeerRegistration,
    incoming: TrackedIncoming,
    state: Arc<PeerState>,
    peer_addr: SocketAddr,
    local_addr: SocketAddr,
    socket: Arc<UdpSocket>,
}
impl UdpServerStream {
    pub(super) fn accepted(
        registration: PeerRegistration,
        incoming: LossyReceiver<QueuedDatagram>,
        state: Arc<PeerState>,
        peer_addr: SocketAddr,
        socket: Arc<UdpSocket>,
        local_addr: SocketAddr,
    ) -> Self {
        Self {
            registration,
            incoming: TrackedIncoming::new(incoming, Arc::clone(&state)),
            state,
            peer_addr,
            local_addr,
            socket,
        }
    }
}
#[async_trait]
impl NetworkStream for UdpServerStream {
    type Handle = UdpConnectionHandle;
    type ReadHalf = UdpReadHalf;
    type WriteHalf = UdpWriteHalf;
    async fn into_split(self) -> io::Result<(Self::ReadHalf, Self::WriteHalf)> {
        let incoming = Incoming::Queue {
            queue: self.incoming,
            current: Box::default(),
            _registration: self.registration,
        };
        Ok(ConnectedPeer {
            incoming,
            socket: self.socket,
            address: Some(self.peer_addr),
            state: self.state,
        }
        .into_split())
    }
    fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }
    fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
    fn transport(&self) -> Self::Handle {
        self.state.handle()
    }
}
impl ServerStream for UdpServerStream {}

/// Local client-side UDP endpoint; setup does not contact the peer.
pub struct ConfiguredUdpClientStream<C: UdpConfig> {
    _config: PhantomData<C>,
    peer_addr: SocketAddr,
    local_addr: SocketAddr,
    socket: Arc<UdpSocket>,
    state: Arc<PeerState>,
}
/// Client stream with default UDP settings.
pub type UdpClientStream = ConfiguredUdpClientStream<DefaultUdpConfig>;

impl<C: UdpConfig> ConfiguredUdpClientStream<C> {
    /// Connects using runtime options instead of `C::OPTIONS`.
    ///
    /// # Errors
    /// Returns an error for invalid options or failed socket setup.
    pub async fn connect_with_options(
        address: SocketAddr,
        options: UdpOptions,
    ) -> io::Result<Self> {
        ValidatedOptions::new(options)?;
        let local: SocketAddr = match address {
            SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
            SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
        };
        let socket = UdpSocket::bind(local).await?;
        socket.connect(address).await?;
        Self::from_socket(socket, options)
    }

    /// Wraps an already connected socket with explicit runtime UDP options.
    /// Bind and configure the socket before connecting to choose an interface,
    /// source port or OS socket options. This constructor sends no datagrams.
    ///
    /// ```no_run
    /// # async fn connect() -> std::io::Result<()> {
    /// use bevy_slinet::protocols::udp::{UdpClientStream, UdpOptions};
    /// let socket = tokio::net::UdpSocket::bind("127.0.0.1:5000").await?;
    /// socket.connect("127.0.0.1:6000").await?;
    /// let stream = UdpClientStream::from_socket(socket, UdpOptions::DEFAULT)?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// # Errors
    /// Returns an error for invalid options, an unconnected socket, or failed
    /// local/peer address queries.
    pub fn from_socket(socket: UdpSocket, options: UdpOptions) -> io::Result<Self> {
        let options = ValidatedOptions::new(options)?;
        let socket = Arc::new(socket);
        let state = PeerState::new(options);
        Ok(Self {
            _config: PhantomData,
            peer_addr: socket.peer_addr()?,
            local_addr: socket.local_addr()?,
            socket,
            state,
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
    type Handle = UdpConnectionHandle;
    type ReadHalf = UdpReadHalf;
    type WriteHalf = UdpWriteHalf;
    async fn into_split(self) -> io::Result<(Self::ReadHalf, Self::WriteHalf)> {
        let incoming = Incoming::Socket {
            socket: Arc::clone(&self.socket),
            buffer: vec![0; BUFFER_SIZE].into_boxed_slice(),
        };
        Ok(ConnectedPeer {
            incoming,
            socket: self.socket,
            address: None,
            state: self.state,
        }
        .into_split())
    }
    fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }
    fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
    fn transport(&self) -> Self::Handle {
        self.state.handle()
    }
}

struct ConnectedPeer {
    incoming: Incoming,
    socket: Arc<UdpSocket>,
    address: Option<SocketAddr>,
    state: Arc<PeerState>,
}
impl ConnectedPeer {
    fn into_split(self) -> (UdpReadHalf, UdpWriteHalf) {
        let Self {
            incoming,
            socket,
            address,
            state,
        } = self;
        (
            UdpReadHalf {
                incoming,
                state: Arc::clone(&state),
            },
            UdpWriteHalf {
                socket,
                address,
                pacer: DataPacer::new(state.handle.rate_updates()),
                state,
            },
        )
    }
}

/// Read half of a local UDP peer; dropping it cancels its writer.
pub struct UdpReadHalf {
    incoming: Incoming,
    state: Arc<PeerState>,
}
impl Drop for UdpReadHalf {
    fn drop(&mut self) {
        self.state.close();
    }
}

#[async_trait]
impl PacketReader for UdpReadHalf {
    fn close(&mut self) {
        self.state.close();
    }

    async fn receive<R, S, Ser, LS>(
        &mut self,
        serializer: Arc<Ser>,
        length: &LS,
        limits: &ReceiveLimits,
    ) -> Result<R, ReceiveError<Ser::DecodeError, LS::Error>>
    where
        R: Send + Sync + Debug + 'static,
        S: Send + Sync + Debug + 'static,
        Ser: Serializer<R, S> + ?Sized,
        LS: PacketLengthSerializer,
    {
        self.receive_with_timestamp(serializer, length, limits)
            .await
            .map(|(packet, _)| packet)
    }
    async fn receive_with_timestamp<R, S, Ser, LS>(
        &mut self,
        serializer: Arc<Ser>,
        _: &LS,
        limits: &ReceiveLimits,
    ) -> Result<(R, Instant), ReceiveError<Ser::DecodeError, LS::Error>>
    where
        R: Send + Sync + Debug + 'static,
        S: Send + Sync + Debug + 'static,
        Ser: Serializer<R, S> + ?Sized,
        LS: PacketLengthSerializer,
    {
        loop {
            let from_socket = matches!(self.incoming, Incoming::Socket { .. });
            let ReceivedDatagram {
                bytes: payload,
                received_at,
            } = tokio::select! {
                biased;
                () = self.state.cancelled() => return Err(ReceiveError::Io(PeerState::disconnected_error())),
                result = self.incoming.next() => result.map_err(|err| {
                    self.state.close();
                    ReceiveError::Io(err)
                })?,
            };
            if from_socket {
                self.state.received_datagram(payload.len());
            }
            if payload.len() > super::MAX_DATAGRAM_SIZE {
                self.state.handle.count_drop(UdpDropReason::ReceiveLimit);
                continue;
            }
            if payload.len() > limits.max_packet_size() {
                self.state.handle.count_drop(UdpDropReason::ReceiveLimit);
                continue;
            }
            match serializer.deserialize(payload) {
                Ok(packet) => return Ok((packet, received_at)),
                Err(err) => {
                    self.state
                        .handle
                        .count_drop(UdpDropReason::MalformedPayload);
                    log::debug!("Dropping malformed UDP payload: {err}");
                }
            }
        }
    }
}

/// Write half of a local UDP peer.
pub struct UdpWriteHalf {
    socket: Arc<UdpSocket>,
    address: Option<SocketAddr>,
    state: Arc<PeerState>,
    pacer: DataPacer,
}
impl UdpWriteHalf {
    /// Number of application packets dropped because they exceeded the configured datagram size.
    #[must_use]
    pub fn dropped_oversized_packets(&self) -> usize {
        self.state.dropped_oversized()
    }
    /// Number of application packets dropped after a socket send error.
    #[must_use]
    pub fn dropped_send_errors(&self) -> usize {
        self.state.dropped_send_errors()
    }

    async fn send_datagram(&self, bytes: &[u8]) -> io::Result<()> {
        if self.state.is_closed() {
            return Err(PeerState::disconnected_error());
        }
        let sent = tokio::select! {
            biased;
            () = self.state.cancelled() => return Err(PeerState::disconnected_error()),
            result = async {
                match self.address {
                    Some(addr) => self.socket.send_to(bytes, addr).await,
                    None => self.socket.send(bytes).await,
                }
            } => result?,
        };
        if sent != bytes.len() {
            return Err(io::Error::from(ErrorKind::WriteZero));
        }
        self.state.handle.data_sent(bytes.len());
        Ok(())
    }
}

#[async_trait]
impl PacketWriter for UdpWriteHalf {
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
        if payload.len() > self.state.options().max_payload_size() {
            self.state.drop_oversized();
            log::warn!("Dropping oversized UDP payload ({} bytes)", payload.len());
            return Ok(());
        }
        self.pacer.wait(self.state.closed_token()).await?;
        let result = self.send_datagram(&payload).await;
        self.pacer.sent(payload.len());
        if let Err(err) = result {
            if self.state.is_closed() {
                return Err(err);
            }
            self.state.drop_send_error();
            log::warn!("Dropping UDP packet: {err}");
        }
        Ok(())
    }
}

#[cfg(test)]
impl UdpReadHalf {
    pub(super) fn queued_bytes(&self) -> usize {
        match &self.incoming {
            Incoming::Queue { queue, .. } => queue.queued_bytes(),
            Incoming::Socket { .. } => 0,
        }
    }
}
#[cfg(test)]
impl UdpWriteHalf {
    #[cfg(target_os = "linux")]
    pub(super) fn set_test_address(&mut self, address: SocketAddr) {
        self.address = Some(address);
    }
}
