use super::handshake::Handshake;
use super::session::{Heartbeat, PeerRegistration, QueuedDatagram, SessionState};
use super::settings::{
    DefaultUdpConfig, UdpConfig, UdpIdleTimeout, UdpOptions, ValidatedOptions, BUFFER_SIZE,
    CONNECT_TIMEOUT,
};
use super::wire::{Control, Frame, Payload};
use crate::{
    connection::MAX_PACKET_SIZE,
    protocols::protocol::{
        ClientStream, NetworkStream, ReadStream, ReceiveError, ServerStream, WriteStream,
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
    sync::{atomic::Ordering, Arc},
    time::Duration,
};
use tokio::{
    net::UdpSocket,
    sync::{mpsc, watch},
};

enum Incoming {
    Queue {
        queue: mpsc::Receiver<QueuedDatagram>,
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
    async fn next(&mut self, state: &SessionState) -> io::Result<ReceivedDatagram<'_>> {
        match self {
            Self::Queue { queue, current, .. } => {
                let QueuedDatagram { bytes, received_at } = queue
                    .recv()
                    .await
                    .ok_or_else(SessionState::disconnected_error)?;
                state.dequeue(bytes.len());
                *current = bytes;
                Ok(ReceivedDatagram {
                    bytes: current,
                    received_at,
                })
            }
            Self::Socket { socket, buffer } => {
                let len = socket.recv(buffer).await?;
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

/// Accepted UDP session.
pub struct UdpServerStream {
    registration: PeerRegistration,
    incoming: mpsc::Receiver<QueuedDatagram>,
    state: Arc<SessionState>,
    peer_addr: SocketAddr,
    local_addr: SocketAddr,
    socket: Arc<UdpSocket>,
}
impl UdpServerStream {
    pub(super) const fn accepted(
        registration: PeerRegistration,
        incoming: mpsc::Receiver<QueuedDatagram>,
        state: Arc<SessionState>,
        peer_addr: SocketAddr,
        socket: Arc<UdpSocket>,
        local_addr: SocketAddr,
    ) -> Self {
        Self {
            registration,
            incoming,
            state,
            peer_addr,
            local_addr,
            socket,
        }
    }
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
        Ok(ConnectedSession {
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
}
impl ServerStream for UdpServerStream {}

/// Connected UDP session.
pub struct ConfiguredUdpClientStream<C: UdpConfig> {
    _config: PhantomData<C>,
    peer_addr: SocketAddr,
    local_addr: SocketAddr,
    socket: Arc<UdpSocket>,
    state: Arc<SessionState>,
}
/// Client stream with default UDP settings.
pub type UdpClientStream = ConfiguredUdpClientStream<DefaultUdpConfig>;

impl<C: UdpConfig> ConfiguredUdpClientStream<C> {
    pub(super) async fn connect_with_options(
        address: SocketAddr,
        options: UdpOptions,
    ) -> io::Result<Self> {
        let options = ValidatedOptions::new(options)?;
        let local: SocketAddr = match address {
            SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
            SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
        };
        let socket = Arc::new(UdpSocket::bind(local).await?);
        socket.connect(address).await?;
        let cookie = tokio::time::timeout(CONNECT_TIMEOUT, Handshake::new(&socket).connect())
            .await
            .map_err(|_| io::Error::new(ErrorKind::TimedOut, "UDP handshake timed out"))??;
        Ok(Self {
            _config: PhantomData,
            peer_addr: socket.peer_addr()?,
            local_addr: socket.local_addr()?,
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
        Ok(ConnectedSession {
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
}

struct ConnectedSession {
    incoming: Incoming,
    socket: Arc<UdpSocket>,
    address: Option<SocketAddr>,
    state: Arc<SessionState>,
}
impl ConnectedSession {
    fn into_split(self) -> (UdpReadHalf, UdpWriteHalf) {
        let Self {
            incoming,
            socket,
            address,
            state,
        } = self;
        Heartbeat::new(Arc::clone(&socket), address, Arc::clone(&state)).spawn();
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
}

/// Read half of a UDP session; dropping it closes the session and stops heartbeats.
pub struct UdpReadHalf {
    incoming: Incoming,
    state: Arc<SessionState>,
    idle_timeout: watch::Receiver<Duration>,
    timeout_updates_open: bool,
}
/// Reads datagrams routed by the listener for an accepted session.
pub type UdpServerReadHalf = UdpReadHalf;
/// Reads datagrams directly from a connected client socket.
pub type UdpClientReadHalf = UdpReadHalf;
impl Drop for UdpReadHalf {
    fn drop(&mut self) {
        self.state.close();
    }
}

#[async_trait]
impl ReadStream for UdpReadHalf {
    fn close(&mut self) {
        self.state.close();
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
            let last = self.state.last_received();
            let deadline = async {
                match last.checked_add(timeout) {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            };
            let from_socket = matches!(self.incoming, Incoming::Socket { .. });
            let ReceivedDatagram { bytes, received_at } = tokio::select! {
                biased;
                () = self.state.cancelled() => return Err(ReceiveError::Io(SessionState::disconnected_error())),
                () = deadline => {
                    // The listener may have received keepalives while this read waited on its queue.
                    if self.state.last_received() != last { continue; }
                    self.state.close();
                    return Err(ReceiveError::Io(io::Error::new(ErrorKind::TimedOut, "no datagrams from the UDP peer")));
                }
                changed = self.idle_timeout.changed(), if self.timeout_updates_open => {
                    self.timeout_updates_open = changed.is_ok(); continue;
                }
                result = self.incoming.next(&self.state) => result.map_err(|err| {
                    self.state.close();
                    ReceiveError::Io(err)
                })?,
            };
            let Some(Frame { session, payload }) = Frame::parse(bytes) else {
                continue;
            };
            if session != self.state.id() || payload == Payload::Control(Control::Accept) {
                continue;
            }
            if from_socket {
                self.state.received();
            }
            if payload == Payload::Control(Control::Disconnect) {
                self.state.close();
                return Err(ReceiveError::Io(SessionState::disconnected_error()));
            }
            let Payload::Data(payload) = payload else {
                continue;
            };
            if payload.len() > MAX_PACKET_SIZE.load(Ordering::Relaxed) {
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
    #[must_use]
    pub fn dropped_oversized_packets(&self) -> usize {
        self.state.dropped_oversized()
    }
    /// Number of application packets dropped after a socket send error.
    #[must_use]
    pub fn dropped_send_errors(&self) -> usize {
        self.state.dropped_send_errors()
    }
}

/// Sends datagrams to an accepted peer through the listener's shared socket.
pub type UdpServerWriteHalf = UdpWriteHalf;
/// Sends datagrams through a connected client socket.
pub type UdpClientWriteHalf = UdpWriteHalf;
#[async_trait]
impl WriteStream for UdpWriteHalf {
    /// Sends raw wire bytes. Prefer `send`, which adds the session header.
    async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.state.is_closed() {
            return Err(SessionState::disconnected_error());
        }
        let sent = match self.address {
            Some(addr) => self.socket.send_to(bytes, addr).await?,
            None => self.socket.send(bytes).await?,
        };
        if sent != bytes.len() {
            return Err(io::Error::from(ErrorKind::WriteZero));
        }
        self.state.sent();
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
        if payload.len() > self.state.options().max_payload_size() {
            self.state.drop_oversized();
            log::warn!("Dropping oversized UDP payload ({} bytes)", payload.len());
            return Ok(());
        }
        if let Err(err) = self
            .write_all(&Frame::data(self.state.id(), &payload).encode())
            .await
        {
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
    pub(super) fn session(&self) -> &SessionState {
        &self.state
    }
}
#[cfg(test)]
impl UdpWriteHalf {
    pub(super) fn session(&self) -> &SessionState {
        &self.state
    }
    pub(super) fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr().unwrap()
    }
}
