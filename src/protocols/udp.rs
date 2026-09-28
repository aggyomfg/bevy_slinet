//! UDP protocol implementation based on [`tokio::net`]. You can enable it by adding `protocol_udp` feature.
//!
//! Every packet is sent as exactly one datagram, without a length prefix. Data datagrams contain
//! a `1` byte followed by the serialized payload, which may be empty. The payload must fit in
//! [`MAX_DATAGRAM_SIZE`] minus one byte (65,506 bytes); larger packets are dropped, as are
//! packets the OS refuses to send. `MaxPacketSize` limits the payload, excluding the tag.
//! The config's `LengthSerializer` is not used. Datagrams larger than the path MTU (about
//! 1,472 bytes of payload on typical networks) are fragmented, and losing any fragment
//! loses the whole packet.
//!
//! Delivery is unreliable and unordered: packets may be lost, duplicated or reordered.
//! A lost or malformed datagram does not affect the framing of other packets. Serializers
//! must decode each datagram independently and remain usable after a decoding error.
//! Serializers that depend on previous packets, such as the example `CustomCryptEngine`
//! stream cipher, are not suitable for UDP.
//!
//! An empty datagram is a connection probe. The server answers every probe with an empty
//! datagram, and [`UdpClientStream::connect`] resends its probe every [`PROBE_INTERVAL`] until
//! it gets an answer or [`CONNECT_TIMEOUT`] passes. Only probes open server connections; other
//! datagrams from unknown peers are answered with a disconnect.
//!
//! Clients send a `2` keep-alive datagram every [`KEEPALIVE_INTERVAL`]. The server sends one per
//! interval only if the client sent something in that interval, so it never sends more
//! datagrams than it receives. Both sides close the connection after [`UdpIdleTimeout`] without
//! any datagrams from the other side. A closing peer sends several `3` disconnect datagrams,
//! which close the connection on the other side.

use std::fmt::Debug;
use std::io;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bevy::log;
use bevy::platform::time::Instant;
use bevy::prelude::Resource;
use dashmap::DashMap;
use tokio::net::UdpSocket;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::MissedTickBehavior;

use crate::connection::MAX_PACKET_SIZE;
use crate::protocol::{
    ClientStream, Listener, NetworkStream, ReadStream, ReceiveError, ServerStream, WriteStream,
};
use crate::serializer::Serializer;
use crate::{PacketLengthSerializer, Protocol};

const BUFFER_SIZE: usize = u16::MAX as usize;
/// The largest UDP payload that can be sent over IPv4.
pub const MAX_DATAGRAM_SIZE: usize = 65_507;
const DATA_DATAGRAM: u8 = 1;
const KEEPALIVE_DATAGRAM: u8 = 2;
const DISCONNECT_DATAGRAM: u8 = 3;
/// Disconnect datagrams are repeated because any of them may be lost.
const DISCONNECT_REPEATS: usize = 3;
/// How often each peer sends a keep-alive datagram.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);
/// How often [`UdpClientStream::connect`] resends its probe until the server answers.
pub const PROBE_INTERVAL: Duration = Duration::from_millis(250);
/// How long [`UdpClientStream::connect`] waits for the server to answer.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Datagrams received from a peer beyond this many unread ones are dropped.
const MAX_QUEUED_DATAGRAMS: usize = 1024;
/// Datagrams that would push a peer's unread bytes above this limit are dropped.
const MAX_QUEUED_BYTES: usize = 1 << 20;

/// Closes a UDP connection when no datagrams arrive from the peer for this long.
/// Must be longer than [`KEEPALIVE_INTERVAL`]; `Duration::MAX` disables it. Defaults to 10 seconds.
/// Applies only to connections in this Bevy app. Changes take effect on the next receive,
/// including after a keep-alive. Removing the resource restores the default.
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

#[cfg(any(feature = "client", feature = "server"))]
pub(crate) fn idle_timeout_receiver(
    world: &bevy::prelude::World,
) -> tokio::sync::watch::Receiver<Duration> {
    let settings = world.resource::<IdleTimeoutSettings>();
    settings.0.send_replace(
        world
            .get_resource::<UdpIdleTimeout>()
            .copied()
            .unwrap_or_default()
            .0,
    );
    settings.0.subscribe()
}

#[cfg(any(feature = "client", feature = "server"))]
pub(crate) fn set_idle_timeout_system(
    timeout: Option<bevy::prelude::Res<UdpIdleTimeout>>,
    settings: bevy::prelude::Res<IdleTimeoutSettings>,
) {
    let timeout = timeout.map(|timeout| *timeout).unwrap_or_default().0;
    settings.0.send_if_modified(|current| {
        if *current == timeout {
            false
        } else {
            *current = timeout;
            true
        }
    });
}

fn idle_error<E, LS>() -> ReceiveError<E, LS>
where
    E: std::error::Error + Send + Sync,
    LS: PacketLengthSerializer,
{
    ReceiveError::Io(io::Error::new(
        ErrorKind::TimedOut,
        "no datagrams from the UDP peer",
    ))
}

async fn send_datagram(
    socket: &UdpSocket,
    peer_addr: Option<SocketAddr>,
    datagram: &[u8],
) -> io::Result<usize> {
    match peer_addr {
        Some(addr) => socket.send_to(datagram, addr).await,
        None => socket.send(datagram).await,
    }
}

/// Sends keep-alive datagrams until the returned sender is dropped, then disconnect datagrams.
/// With `peer`, a keep-alive is sent only if a datagram arrived from it since the previous one,
/// so spoofed probes are not amplified.
fn spawn_keepalive(
    socket: Arc<UdpSocket>,
    peer_addr: Option<SocketAddr>,
    peer: Option<Arc<PeerState>>,
) -> oneshot::Sender<()> {
    let (stop, mut stopped) = oneshot::channel();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(KEEPALIVE_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = &mut stopped => break,
                _ = interval.tick() => {
                    if peer.as_ref().is_some_and(|peer| !peer.seen.swap(false, Ordering::Relaxed)) {
                        continue;
                    }
                    // Errors are reported to the read half or end in an idle timeout.
                    let _ = send_datagram(&socket, peer_addr, &[KEEPALIVE_DATAGRAM]).await;
                }
            }
        }
        for _ in 0..DISCONNECT_REPEATS {
            let _ = send_datagram(&socket, peer_addr, &[DISCONNECT_DATAGRAM]).await;
        }
    });
    stop
}

/// UDP protocol.
pub struct UdpProtocol;

#[async_trait]
impl Protocol for UdpProtocol {
    type Listener = UdpNetworkListener;
    type ServerStream = UdpServerStream;
    type ClientStream = UdpClientStream;

    const DATAGRAM: bool = true;

    async fn bind(addr: SocketAddr) -> std::io::Result<Self::Listener> {
        Ok(UdpNetworkListener {
            socket: Arc::new(UdpSocket::bind(addr).await?),
            tasks: DashMap::new(),
        })
    }
}

type Queued = (Box<[u8]>, Instant);

#[derive(Default)]
struct PeerState {
    queued_bytes: AtomicUsize,
    /// Set for every non-probe datagram, cleared by each keep-alive tick.
    seen: AtomicBool,
}

/// The listener's end of a server connection's datagram queue.
struct Peer {
    queue: mpsc::Sender<Queued>,
    state: Arc<PeerState>,
}

impl Peer {
    fn new() -> (Self, mpsc::Receiver<Queued>) {
        let (queue, incoming) = mpsc::channel(MAX_QUEUED_DATAGRAMS);
        let peer = Peer {
            queue,
            state: Arc::default(),
        };
        (peer, incoming)
    }

    /// Returns `false` if the connection's read half is gone.
    fn push(&self, datagram: &[u8], received_at: Instant) -> bool {
        // Empty datagrams are connection probes sent by `UdpClientStream::connect`.
        let len = datagram.len();
        if len > 0 {
            self.state.seen.store(true, Ordering::Relaxed);
        }
        if len == 0 || self.state.queued_bytes.load(Ordering::Relaxed) + len > MAX_QUEUED_BYTES {
            return !self.queue.is_closed();
        }
        self.state.queued_bytes.fetch_add(len, Ordering::Relaxed);
        match self.queue.try_send((datagram.into(), received_at)) {
            Ok(()) => true,
            Err(err) => {
                self.state.queued_bytes.fetch_sub(len, Ordering::Relaxed);
                matches!(err, TrySendError::Full(_))
            }
        }
    }
}

enum Incoming {
    Queue {
        queue: mpsc::Receiver<Queued>,
        state: Arc<PeerState>,
        current: Box<[u8]>,
    },
    Socket {
        socket: Arc<UdpSocket>,
        buffer: Box<[u8]>,
    },
}

impl Incoming {
    async fn next(&mut self) -> io::Result<(&[u8], Instant)> {
        match self {
            Incoming::Queue {
                queue,
                state,
                current,
            } => {
                let (datagram, received_at) = queue.recv().await.ok_or_else(disconnected_error)?;
                state
                    .queued_bytes
                    .fetch_sub(datagram.len(), Ordering::Relaxed);
                *current = datagram;
                Ok((current, received_at))
            }
            Incoming::Socket { socket, buffer } => {
                let len = socket.recv(buffer).await?;
                Ok((&buffer[..len], Instant::now()))
            }
        }
    }
}

/// A UDP listener.
pub struct UdpNetworkListener {
    socket: Arc<UdpSocket>,
    tasks: DashMap<SocketAddr, Peer>,
}

#[async_trait]
impl Listener for UdpNetworkListener {
    type Stream = UdpServerStream;

    async fn accept(&self) -> std::io::Result<UdpServerStream> {
        let mut buf = [0; BUFFER_SIZE];
        loop {
            let (len, address) = match self.socket.recv_from(&mut buf).await {
                Ok(received) => received,
                // Windows reports ICMP "port unreachable" for an earlier `send_to` here.
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
            let received_at = Instant::now();
            let datagram = &buf[..len];
            let delivered = self
                .tasks
                .get(&address)
                .map(|peer| peer.push(datagram, received_at));
            if delivered == Some(false) {
                self.tasks.remove(&address);
            }
            if datagram.is_empty() {
                // Answered before the connection exists, so a cancelled `accept` loses only the probe.
                let _ = self.socket.send_to(&[], address).await;
                if delivered != Some(true) {
                    let (peer, incoming) = Peer::new();
                    let state = Arc::clone(&peer.state);
                    self.tasks.insert(address, peer);
                    return Ok(UdpServerStream {
                        incoming,
                        state,
                        peer_addr: address,
                        socket: Arc::clone(&self.socket),
                    });
                }
            } else if delivered != Some(true) && datagram[0] != DISCONNECT_DATAGRAM {
                // Tells peers of closed connections to stop. The answer is never larger.
                let _ = self.socket.send_to(&[DISCONNECT_DATAGRAM], address).await;
            }
        }
    }

    fn address(&self) -> SocketAddr {
        self.socket.local_addr().unwrap()
    }

    fn handle_disconnection(&self, peer_addr: SocketAddr) {
        self.tasks.remove(&peer_addr);
    }
}

/// A server-side UDP connection that reads the datagrams queued by [`UdpNetworkListener`].
pub struct UdpServerStream {
    incoming: mpsc::Receiver<Queued>,
    state: Arc<PeerState>,
    peer_addr: SocketAddr,
    socket: Arc<UdpSocket>,
}

#[async_trait]
impl NetworkStream for UdpServerStream {
    type ReadHalf = UdpServerReadHalf;
    type WriteHalf = UdpServerWriteHalf;

    async fn into_split(self) -> io::Result<(Self::ReadHalf, Self::WriteHalf)> {
        let peer_addr = Some(self.peer_addr);
        let keepalive = spawn_keepalive(
            Arc::clone(&self.socket),
            peer_addr,
            Some(Arc::clone(&self.state)),
        );
        let incoming = Incoming::Queue {
            queue: self.incoming,
            state: self.state,
            current: Box::default(),
        };
        Ok((
            UdpReadHalf::new(incoming, keepalive),
            UdpWriteHalf {
                socket: self.socket,
                peer_addr,
            },
        ))
    }

    fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr().unwrap()
    }
}

/// The read half of a UDP connection.
pub struct UdpReadHalf {
    incoming: Incoming,
    idle_timeout: watch::Receiver<Duration>,
    _keepalive: oneshot::Sender<()>,
}

/// The read half of [`UdpServerStream`].
pub type UdpServerReadHalf = UdpReadHalf;
/// The read half of [`UdpClientStream`].
pub type UdpClientReadHalf = UdpReadHalf;

impl UdpReadHalf {
    fn new(incoming: Incoming, keepalive: oneshot::Sender<()>) -> Self {
        UdpReadHalf {
            incoming,
            idle_timeout: watch::channel(UdpIdleTimeout::default().0).1,
            _keepalive: keepalive,
        }
    }
}

#[async_trait]
impl ReadStream for UdpReadHalf {
    fn set_idle_timeout(&mut self, timeout: watch::Receiver<Duration>) {
        self.idle_timeout = timeout;
    }

    async fn read_exact(&mut self, _buffer: &mut [u8]) -> io::Result<()> {
        Err(datagram_only_error())
    }

    async fn receive<ReceivingPacket, SendingPacket, S, LS>(
        &mut self,
        serializer: Arc<S>,
        length_serializer: &LS,
    ) -> Result<ReceivingPacket, ReceiveError<S::DecodeError, LS>>
    where
        ReceivingPacket: Send + Sync + Debug + 'static,
        SendingPacket: Send + Sync + Debug + 'static,
        S: Serializer<ReceivingPacket, SendingPacket> + ?Sized,
        LS: PacketLengthSerializer,
    {
        self.receive_with_timestamp(serializer, length_serializer)
            .await
            .map(|(packet, _)| packet)
    }

    async fn receive_with_timestamp<ReceivingPacket, SendingPacket, S, LS>(
        &mut self,
        serializer: Arc<S>,
        _length_serializer: &LS,
    ) -> Result<(ReceivingPacket, Instant), ReceiveError<S::DecodeError, LS>>
    where
        ReceivingPacket: Send + Sync + Debug + 'static,
        SendingPacket: Send + Sync + Debug + 'static,
        S: Serializer<ReceivingPacket, SendingPacket> + ?Sized,
        LS: PacketLengthSerializer,
    {
        loop {
            let idle_timeout = *self.idle_timeout.borrow();
            let (datagram, received_at) = tokio::time::timeout(idle_timeout, self.incoming.next())
                .await
                .map_err(|_| idle_error())?
                .map_err(ReceiveError::Io)?;
            if let Some(packet) = decode_datagram(
                datagram,
                &*serializer,
                MAX_PACKET_SIZE.load(Ordering::Relaxed),
            )
            .map_err(ReceiveError::Io)?
            {
                return Ok((packet, received_at));
            }
        }
    }
}

/// The write half of a UDP connection.
pub struct UdpWriteHalf {
    socket: Arc<UdpSocket>,
    /// `None` for connected client sockets.
    peer_addr: Option<SocketAddr>,
}

/// The write half of [`UdpServerStream`].
pub type UdpServerWriteHalf = UdpWriteHalf;
/// The write half of [`UdpClientStream`].
pub type UdpClientWriteHalf = UdpWriteHalf;

#[async_trait]
impl WriteStream for UdpWriteHalf {
    async fn write_all(&mut self, buffer: &[u8]) -> std::io::Result<()> {
        send_datagram(&self.socket, self.peer_addr, buffer)
            .await
            .and_then(|i| assert_all(i, buffer))
    }

    async fn send<ReceivingPacket, SendingPacket, S, LS>(
        &mut self,
        packet: SendingPacket,
        serializer: Arc<S>,
        _length_serializer: &LS,
    ) -> io::Result<()>
    where
        ReceivingPacket: Send + Sync + Debug + 'static,
        SendingPacket: Send + Sync + Debug + 'static,
        S: Serializer<ReceivingPacket, SendingPacket> + ?Sized,
        LS: PacketLengthSerializer,
    {
        match encode_datagram(packet, &*serializer) {
            Some(datagram) => drop_on_error(self.write_all(&datagram).await, &datagram),
            None => Ok(()),
        }
    }
}

impl ServerStream for UdpServerStream {}

/// A UDP client stream.
pub struct UdpClientStream {
    socket: Arc<UdpSocket>,
    peer_addr: SocketAddr,
}

#[async_trait]
impl NetworkStream for UdpClientStream {
    type ReadHalf = UdpClientReadHalf;
    type WriteHalf = UdpClientWriteHalf;

    async fn into_split(self) -> io::Result<(Self::ReadHalf, Self::WriteHalf)> {
        let incoming = Incoming::Socket {
            socket: Arc::clone(&self.socket),
            buffer: vec![0; BUFFER_SIZE].into_boxed_slice(),
        };
        let keepalive = spawn_keepalive(Arc::clone(&self.socket), None, None);
        let write = UdpWriteHalf {
            socket: self.socket,
            peer_addr: None,
        };
        Ok((UdpReadHalf::new(incoming, keepalive), write))
    }

    fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr().unwrap()
    }
}

#[async_trait]
impl ClientStream for UdpClientStream {
    async fn connect(addr: SocketAddr) -> std::io::Result<Self>
    where
        Self: Sized,
    {
        let local_addr: SocketAddr = match addr {
            SocketAddr::V4(_) => (Ipv4Addr::UNSPECIFIED, 0).into(),
            SocketAddr::V6(_) => (Ipv6Addr::UNSPECIFIED, 0).into(),
        };
        let socket = UdpSocket::bind(local_addr).await?;
        socket.connect(addr).await?;
        let peer_addr = socket.peer_addr()?;
        tokio::time::timeout(CONNECT_TIMEOUT, handshake(&socket))
            .await
            .map_err(|_| io::Error::new(ErrorKind::TimedOut, "the UDP server did not answer"))??;
        Ok(UdpClientStream {
            socket: Arc::new(socket),
            peer_addr,
        })
    }
}

/// Probes the server until it answers. Only a probe answer is consumed; any other datagram
/// also proves the server knows this client and is left for the read half.
async fn handshake(socket: &UdpSocket) -> io::Result<()> {
    let mut buffer = vec![0; BUFFER_SIZE];
    loop {
        socket.send(&[]).await?;
        if let Ok(answer) = tokio::time::timeout(PROBE_INTERVAL, socket.peek(&mut buffer)).await {
            if answer? == 0 {
                socket.recv(&mut buffer).await?;
            }
            return Ok(());
        }
    }
}

/// UDP send errors (`EMSGSIZE`, `ENOBUFS`, ICMP errors) affect one packet, not the connection.
fn drop_on_error(result: io::Result<()>, datagram: &[u8]) -> io::Result<()> {
    if let Err(err) = result {
        log::warn!("Dropping a {}-byte UDP packet: {err}", datagram.len());
    }
    Ok(())
}

fn assert_all(i: usize, buf: &[u8]) -> io::Result<()> {
    if i == buf.len() {
        Ok(())
    } else {
        Err(io::Error::from(ErrorKind::BrokenPipe))
    }
}

fn datagram_only_error() -> io::Error {
    io::Error::new(
        ErrorKind::Unsupported,
        "UDP streams are datagram-based, use `ReadStream::receive`",
    )
}

fn disconnected_error() -> io::Error {
    io::Error::new(ErrorKind::ConnectionAborted, "the UDP peer disconnected")
}

/// Returns `Ok(None)` for datagrams that should be dropped without closing the connection.
fn decode_datagram<ReceivingPacket, SendingPacket, S>(
    datagram: &[u8],
    serializer: &S,
    max_packet_size: usize,
) -> io::Result<Option<ReceivingPacket>>
where
    ReceivingPacket: Send + Sync + Debug + 'static,
    SendingPacket: Send + Sync + Debug + 'static,
    S: Serializer<ReceivingPacket, SendingPacket> + ?Sized,
{
    let Some((&tag, payload)) = datagram.split_first() else {
        return Ok(None);
    };
    if tag == DISCONNECT_DATAGRAM {
        return Err(disconnected_error());
    }
    if tag != DATA_DATAGRAM || datagram.len() > MAX_DATAGRAM_SIZE {
        return Ok(None);
    }
    if payload.len() > max_packet_size {
        log::debug!(
            "Dropping a {}-byte payload larger than MaxPacketSize",
            payload.len()
        );
        return Ok(None);
    }
    match serializer.deserialize(payload) {
        Ok(packet) => Ok(Some(packet)),
        Err(err) => {
            log::debug!("Dropping a malformed datagram: {err}");
            Ok(None)
        }
    }
}

fn encode_datagram<ReceivingPacket, SendingPacket, S>(
    packet: SendingPacket,
    serializer: &S,
) -> Option<Vec<u8>>
where
    ReceivingPacket: Send + Sync + Debug + 'static,
    SendingPacket: Send + Sync + Debug + 'static,
    S: Serializer<ReceivingPacket, SendingPacket> + ?Sized,
{
    let payload = serializer
        .serialize(packet)
        .expect("Error serializing packet");
    if payload.len() >= MAX_DATAGRAM_SIZE {
        log::warn!(
            "Dropping a {}-byte packet: UDP payloads must be smaller than {MAX_DATAGRAM_SIZE} bytes",
            payload.len()
        );
        return None;
    }
    let mut datagram = Vec::with_capacity(payload.len() + 1);
    datagram.push(DATA_DATAGRAM);
    datagram.extend_from_slice(&payload);
    Some(datagram)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet_length_serializer::LittleEndian;
    use std::future::Future;
    use std::time::Duration;

    /// Passes bytes through as-is; payloads starting with `0xFF` are treated as malformed.
    struct RawSerializer;

    impl Serializer<Vec<u8>, Vec<u8>> for RawSerializer {
        type EncodeError = io::Error;
        type DecodeError = io::Error;

        fn serialize(&self, packet: Vec<u8>) -> io::Result<Vec<u8>> {
            Ok(packet)
        }

        fn deserialize(&self, data: &[u8]) -> io::Result<Vec<u8>> {
            match data.first() {
                Some(0xFF) => Err(ErrorKind::InvalidData.into()),
                _ => Ok(data.to_vec()),
            }
        }
    }

    type Ls = LittleEndian<u32>;

    async fn with_timeout<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(5), future)
            .await
            .expect("UDP test timed out")
    }

    async fn connected() -> (
        Arc<UdpNetworkListener>,
        UdpClientReadHalf,
        UdpClientWriteHalf,
        UdpServerReadHalf,
        UdpServerWriteHalf,
    ) {
        let listener = Arc::new(UdpProtocol::bind(([127, 0, 0, 1], 0).into()).await.unwrap());
        let (client, server) = tokio::join!(
            UdpProtocol::connect_to_server(listener.address()),
            listener.accept()
        );
        let (client, server) = (client.unwrap(), server.unwrap());
        assert_eq!(server.peer_addr(), client.local_addr());
        let pump = Arc::clone(&listener);
        tokio::spawn(async move { while pump.accept().await.is_ok() {} });

        let (client_read, client_write) = client.into_split().await.unwrap();
        let (server_read, server_write) = server.into_split().await.unwrap();
        (
            listener,
            client_read,
            client_write,
            server_read,
            server_write,
        )
    }

    async fn connected_pair() -> (
        UdpClientReadHalf,
        UdpClientWriteHalf,
        UdpServerReadHalf,
        UdpServerWriteHalf,
    ) {
        let (_, client_read, client_write, server_read, server_write) = connected().await;
        (client_read, client_write, server_read, server_write)
    }

    /// Connects a client to a raw socket that answers the first probe with `answer`.
    async fn connect_to_raw(server: &UdpSocket, answer: &[u8]) -> UdpClientStream {
        let (client, ()) = tokio::join!(
            UdpClientStream::connect(server.local_addr().unwrap()),
            async {
                let (_, client) = server.recv_from(&mut [0; 16]).await.unwrap();
                server.send_to(answer, client).await.unwrap();
            }
        );
        client.unwrap()
    }

    fn assert_disconnected(err: ReceiveError<io::Error, Ls>) {
        assert!(
            matches!(err, ReceiveError::Io(ref err) if err.kind() == ErrorKind::ConnectionAborted),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn each_datagram_is_one_packet() {
        with_timeout(async {
            let serializer = Arc::new(RawSerializer);
            let ls = Ls::default();
            let (mut client_read, mut client_write, mut server_read, mut server_write) =
                connected_pair().await;

            // The oversized packet is dropped on send, the 0xFF one on receive.
            for packet in [vec![1], vec![], vec![0; MAX_DATAGRAM_SIZE], vec![2, 3]] {
                client_write
                    .send::<Vec<u8>, _, _, _>(packet, Arc::clone(&serializer), &ls)
                    .await
                    .unwrap();
            }
            client_write
                .write_all(&[DATA_DATAGRAM, 0xFF, 9])
                .await
                .unwrap();
            client_write
                .send::<Vec<u8>, _, _, _>(vec![4], Arc::clone(&serializer), &ls)
                .await
                .unwrap();

            for expected in [vec![1], vec![], vec![2, 3], vec![4]] {
                let packet = server_read
                    .receive::<_, Vec<u8>, _, _>(Arc::clone(&serializer), &ls)
                    .await
                    .unwrap();
                assert_eq!(packet, expected);
            }

            for expected in [vec![], vec![5]] {
                server_write
                    .send::<Vec<u8>, _, _, _>(expected.clone(), Arc::clone(&serializer), &ls)
                    .await
                    .unwrap();
                let packet = client_read
                    .receive::<_, Vec<u8>, _, _>(Arc::clone(&serializer), &ls)
                    .await
                    .unwrap();
                assert_eq!(packet, expected);
            }
        })
        .await;
    }

    #[tokio::test]
    async fn unknown_peers_are_told_to_disconnect() {
        with_timeout(async {
            let listener = UdpProtocol::bind(([127, 0, 0, 1], 0).into()).await.unwrap();
            let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            peer.send_to(&[DATA_DATAGRAM, 7], listener.address())
                .await
                .unwrap();
            peer.send_to(&[], listener.address()).await.unwrap();

            let stream = listener.accept().await.unwrap();
            assert_eq!(stream.peer_addr(), peer.local_addr().unwrap());
            let mut buf = [0; 16];
            for expected in [&[DISCONNECT_DATAGRAM][..], &[]] {
                let (len, _) = peer.recv_from(&mut buf).await.unwrap();
                assert_eq!(&buf[..len], expected);
            }
            let (mut read, _) = stream.into_split().await.unwrap();
            drop(listener);
            let err = read.incoming.next().await.unwrap_err();
            assert_eq!(err.kind(), ErrorKind::ConnectionAborted);
        })
        .await;
    }

    fn queued_read_half() -> (Peer, UdpReadHalf) {
        let (peer, queue) = Peer::new();
        let incoming = Incoming::Queue {
            queue,
            state: Arc::clone(&peer.state),
            current: Box::default(),
        };
        (peer, UdpReadHalf::new(incoming, oneshot::channel().0))
    }

    async fn next_queued(read: &mut UdpReadHalf) -> Vec<u8> {
        read.incoming.next().await.unwrap().0.to_vec()
    }

    #[tokio::test]
    async fn queued_packets_preserve_their_timestamps() {
        let (peer, mut read) = queued_read_half();
        peer.push(&[KEEPALIVE_DATAGRAM], Instant::now());
        peer.push(&[DATA_DATAGRAM, 0xFF], Instant::now());
        let first = Instant::now();
        peer.push(&[DATA_DATAGRAM, 7], first);
        let second = Instant::now();
        peer.push(&[DATA_DATAGRAM], second);
        for expected in [(vec![7], first), (vec![], second)] {
            let received = read
                .receive_with_timestamp::<_, Vec<u8>, _, _>(Arc::new(RawSerializer), &Ls::default())
                .await
                .unwrap();
            assert_eq!(received, expected);
        }
    }

    #[tokio::test]
    async fn unknown_datagrams_do_not_open_connections() {
        with_timeout(async {
            let listener = UdpProtocol::bind(([127, 0, 0, 1], 0).into()).await.unwrap();
            let garbage = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            for datagram in [
                &[0][..],
                &[0xAB, 7],
                &[KEEPALIVE_DATAGRAM],
                &[DATA_DATAGRAM, 7],
                &[DISCONNECT_DATAGRAM],
            ] {
                garbage.send_to(datagram, listener.address()).await.unwrap();
            }
            peer.send_to(&[], listener.address()).await.unwrap();

            let stream = listener.accept().await.unwrap();
            assert_eq!(stream.peer_addr(), peer.local_addr().unwrap());
            assert!(!listener.tasks.contains_key(&garbage.local_addr().unwrap()));
        })
        .await;
    }

    fn decode(datagram: &[u8], max_packet_size: usize) -> Option<Vec<u8>> {
        decode_datagram(datagram, &RawSerializer, max_packet_size).unwrap()
    }

    #[test]
    fn datagram_size_limits() {
        let payload = vec![7; MAX_DATAGRAM_SIZE - 1];
        let datagram = encode_datagram(payload.clone(), &RawSerializer).unwrap();
        assert_eq!(datagram.len(), MAX_DATAGRAM_SIZE);
        assert_eq!(datagram[0], DATA_DATAGRAM);
        assert_eq!(decode(&datagram, payload.len()), Some(payload.clone()));
        assert_eq!(decode(&datagram, payload.len() - 1), None);
        assert!(encode_datagram(vec![7; MAX_DATAGRAM_SIZE], &RawSerializer).is_none());
        assert_eq!(
            decode(&vec![DATA_DATAGRAM; MAX_DATAGRAM_SIZE + 1], usize::MAX),
            None
        );
        assert_eq!(
            encode_datagram(vec![], &RawSerializer),
            Some(vec![DATA_DATAGRAM])
        );
        assert_eq!(decode(&[DATA_DATAGRAM], 0), Some(vec![]));
        assert_eq!(decode(&[DATA_DATAGRAM, 7], 0), None);
        assert_eq!(decode(&[], 0), None);
        assert_eq!(decode(&[KEEPALIVE_DATAGRAM], usize::MAX), None);
        assert_eq!(decode(&[0xAB, 7], usize::MAX), None);
        let err = decode_datagram(&[DISCONNECT_DATAGRAM], &RawSerializer, usize::MAX).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ConnectionAborted);
    }

    #[tokio::test]
    async fn send_errors_drop_only_the_packet() {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let mut write = UdpWriteHalf {
            socket,
            peer_addr: Some((Ipv6Addr::LOCALHOST, 1).into()),
        };
        assert!(write.write_all(&[DATA_DATAGRAM]).await.is_err());
        write
            .send::<Vec<u8>, _, _, _>(vec![7], Arc::new(RawSerializer), &Ls::default())
            .await
            .unwrap();
    }

    #[test]
    fn packet_size_limit_drops_only_oversized_payloads() {
        let datagrams = [
            vec![DATA_DATAGRAM, 1],
            vec![DATA_DATAGRAM, 2, 3, 4],
            vec![DATA_DATAGRAM],
            vec![DATA_DATAGRAM, 5, 6],
        ];
        let received: Vec<_> = datagrams
            .iter()
            .filter_map(|datagram| decode(datagram, 2))
            .collect();
        assert_eq!(received, [vec![1], vec![], vec![5, 6]]);
    }

    #[tokio::test]
    async fn invalid_datagrams_do_not_affect_following_packets() {
        async fn check(read: &mut impl ReadStream, write: &mut impl WriteStream) {
            for invalid in [
                vec![],
                vec![0],
                vec![KEEPALIVE_DATAGRAM],
                vec![0xAB, 7],
                vec![DATA_DATAGRAM, 0xFF, 9],
            ] {
                write.write_all(&invalid).await.unwrap();
            }
            write
                .send::<Vec<u8>, _, _, _>(vec![7], Arc::new(RawSerializer), &Ls::default())
                .await
                .unwrap();
            let received = read
                .receive::<_, Vec<u8>, _, _>(Arc::new(RawSerializer), &Ls::default())
                .await
                .unwrap();
            assert_eq!(received, vec![7]);
        }

        with_timeout(async {
            let (mut client_read, mut client_write, mut server_read, mut server_write) =
                connected_pair().await;
            check(&mut server_read, &mut client_write).await;
            check(&mut client_read, &mut server_write).await;
        })
        .await;
    }

    #[tokio::test]
    async fn loss_reordering_and_duplicates_do_not_affect_decoding() {
        async fn check(read: &mut impl ReadStream, write: &mut impl WriteStream) {
            for sequence in [3, 1, 3, 5] {
                let packet = vec![sequence; usize::from(sequence)];
                write
                    .send::<Vec<u8>, _, _, _>(
                        packet.clone(),
                        Arc::new(RawSerializer),
                        &Ls::default(),
                    )
                    .await
                    .unwrap();
                let received = read
                    .receive::<_, Vec<u8>, _, _>(Arc::new(RawSerializer), &Ls::default())
                    .await
                    .unwrap();
                assert_eq!(received, packet);
            }
        }

        with_timeout(async {
            let (mut client_read, mut client_write, mut server_read, mut server_write) =
                connected_pair().await;
            check(&mut server_read, &mut client_write).await;
            check(&mut client_read, &mut server_write).await;
        })
        .await;
    }

    #[tokio::test]
    async fn queue_overflow_drops_only_excess_datagrams() {
        with_timeout(async {
            let (peer, mut read) = queued_read_half();
            assert!(peer.push(&[], Instant::now()));
            assert_eq!(peer.queue.capacity(), MAX_QUEUED_DATAGRAMS);
            for _ in 0..MAX_QUEUED_DATAGRAMS {
                assert!(peer.push(&[DATA_DATAGRAM, 7], Instant::now()));
            }
            assert!(peer.push(&[DATA_DATAGRAM, 8], Instant::now()));
            assert_eq!(peer.queue.capacity(), 0);
            assert_eq!(next_queued(&mut read).await, [DATA_DATAGRAM, 7]);
            peer.push(&[DATA_DATAGRAM, 9], Instant::now());
            for _ in 1..MAX_QUEUED_DATAGRAMS {
                assert_eq!(next_queued(&mut read).await, [DATA_DATAGRAM, 7]);
            }
            assert_eq!(next_queued(&mut read).await, [DATA_DATAGRAM, 9]);
            assert_eq!(peer.state.queued_bytes.load(Ordering::Relaxed), 0);
            let mut pending = Box::pin(next_queued(&mut read));
            assert!(futures::poll!(pending.as_mut()).is_pending());
            peer.push(&[DATA_DATAGRAM], Instant::now());
            assert_eq!(pending.await, [DATA_DATAGRAM]);
            drop(read);
            assert!(!peer.push(&[DATA_DATAGRAM], Instant::now()));
        })
        .await;
    }

    #[tokio::test]
    async fn queue_byte_budget_drops_only_excess_datagrams() {
        with_timeout(async {
            let (peer, mut read) = queued_read_half();
            let queued_bytes = || peer.state.queued_bytes.load(Ordering::Relaxed);
            let big = vec![DATA_DATAGRAM; MAX_DATAGRAM_SIZE];
            let fits = MAX_QUEUED_BYTES / MAX_DATAGRAM_SIZE;
            for _ in 0..fits + 1 {
                peer.push(&big, Instant::now());
            }
            let small_fits = MAX_QUEUED_BYTES - fits * MAX_DATAGRAM_SIZE;
            peer.push(&vec![DATA_DATAGRAM; small_fits], Instant::now());
            peer.push(&[DATA_DATAGRAM], Instant::now());
            assert_eq!(MAX_QUEUED_DATAGRAMS - peer.queue.capacity(), fits + 1);
            assert_eq!(queued_bytes(), MAX_QUEUED_BYTES);
            assert_eq!(next_queued(&mut read).await.len(), MAX_DATAGRAM_SIZE);
            peer.push(&big, Instant::now());
            assert_eq!(queued_bytes(), MAX_QUEUED_BYTES);
        })
        .await;
    }

    async fn next_non_probe(socket: &UdpSocket) -> Vec<u8> {
        let mut buf = [0; 16];
        loop {
            let (len, _) = socket.recv_from(&mut buf).await.unwrap();
            if len > 0 {
                return buf[..len].to_vec();
            }
        }
    }

    fn assert_idle(err: ReceiveError<io::Error, Ls>) {
        assert!(matches!(err, ReceiveError::Io(err) if err.kind() == ErrorKind::TimedOut));
    }

    #[tokio::test]
    async fn both_sides_send_keepalives() {
        with_timeout(async {
            let listener = Arc::new(UdpProtocol::bind(([127, 0, 0, 1], 0).into()).await.unwrap());
            let client_peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            client_peer.send_to(&[], listener.address()).await.unwrap();
            let _server = listener.accept().await.unwrap().into_split().await.unwrap();
            let pump = Arc::clone(&listener);
            tokio::spawn(async move { while pump.accept().await.is_ok() {} });
            client_peer
                .send_to(&[KEEPALIVE_DATAGRAM], listener.address())
                .await
                .unwrap();
            assert_eq!(next_non_probe(&client_peer).await, [KEEPALIVE_DATAGRAM]);

            let server_peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let client = connect_to_raw(&server_peer, &[]).await;
            let _client = client.into_split().await.unwrap();
            assert_eq!(next_non_probe(&server_peer).await, [KEEPALIVE_DATAGRAM]);
        })
        .await;
    }

    fn drain(socket: &UdpSocket) -> Vec<Vec<u8>> {
        let mut buf = [0; 16];
        let mut received = Vec::new();
        while let Ok((len, _)) = socket.try_recv_from(&mut buf) {
            received.push(buf[..len].to_vec());
        }
        received
    }

    #[tokio::test(start_paused = true)]
    async fn server_keepalives_only_answer_client_traffic() {
        let listener = Arc::new(UdpProtocol::bind(([127, 0, 0, 1], 0).into()).await.unwrap());
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        peer.send_to(&[], listener.address()).await.unwrap();
        let _server = listener.accept().await.unwrap().into_split().await.unwrap();
        let pump = Arc::clone(&listener);
        tokio::spawn(async move { while pump.accept().await.is_ok() {} });
        tokio::time::sleep(KEEPALIVE_INTERVAL * 5).await;
        assert_eq!(drain(&peer), [Vec::<u8>::new()]);

        peer.send_to(&[KEEPALIVE_DATAGRAM], listener.address())
            .await
            .unwrap();
        tokio::time::sleep(KEEPALIVE_INTERVAL * 5).await;
        assert_eq!(drain(&peer), [vec![KEEPALIVE_DATAGRAM]]);
    }

    #[tokio::test(start_paused = true)]
    async fn keepalives_stop_with_the_read_half() {
        let listener = UdpProtocol::bind(([127, 0, 0, 1], 0).into()).await.unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        peer.send_to(&[], listener.address()).await.unwrap();
        let (read, _write) = listener.accept().await.unwrap().into_split().await.unwrap();
        drop(read);
        tokio::time::sleep(KEEPALIVE_INTERVAL * 5).await;
        let received = drain(&peer);
        assert!(
            received.ends_with(&vec![vec![DISCONNECT_DATAGRAM]; DISCONNECT_REPEATS]),
            "{received:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn idle_peers_time_out() {
        let serializer = Arc::new(RawSerializer);
        let listener = UdpProtocol::bind(([127, 0, 0, 1], 0).into()).await.unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        peer.send_to(&[], listener.address()).await.unwrap();
        let (mut server_read, _server_write) =
            listener.accept().await.unwrap().into_split().await.unwrap();
        let err = server_read
            .receive::<Vec<u8>, Vec<u8>, _, _>(Arc::clone(&serializer), &Ls::default())
            .await
            .unwrap_err();
        assert_idle(err);

        let server_peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client = connect_to_raw(&server_peer, &[]).await;
        let (mut client_read, _client_write) = client.into_split().await.unwrap();
        let err = client_read
            .receive::<Vec<u8>, Vec<u8>, _, _>(serializer, &Ls::default())
            .await
            .unwrap_err();
        assert_idle(err);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_timeouts_are_per_connection() {
        let listener = UdpProtocol::bind(([127, 0, 0, 1], 0).into()).await.unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        peer.send_to(&[], listener.address()).await.unwrap();
        let (mut server_read, _server_write) =
            listener.accept().await.unwrap().into_split().await.unwrap();
        let server_peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let client = connect_to_raw(&server_peer, &[]).await;
        let (mut client_read, _client_write) = client.into_split().await.unwrap();
        server_read.set_idle_timeout(tokio::sync::watch::channel(Duration::from_secs(2)).1);
        client_read.set_idle_timeout(tokio::sync::watch::channel(Duration::from_secs(4)).1);

        let started = tokio::time::Instant::now();
        let (server_elapsed, client_elapsed) = tokio::join!(
            async {
                assert_idle(
                    server_read
                        .receive::<Vec<u8>, Vec<u8>, _, _>(Arc::new(RawSerializer), &Ls::default())
                        .await
                        .unwrap_err(),
                );
                started.elapsed()
            },
            async {
                assert_idle(
                    client_read
                        .receive::<Vec<u8>, Vec<u8>, _, _>(Arc::new(RawSerializer), &Ls::default())
                        .await
                        .unwrap_err(),
                );
                started.elapsed()
            }
        );
        assert_eq!(server_elapsed, Duration::from_secs(2));
        assert_eq!(client_elapsed, Duration::from_secs(4));
    }

    #[tokio::test(start_paused = true)]
    async fn idle_timeout_updates_after_keepalive() {
        let (peer, mut read) = queued_read_half();
        let (settings, timeout) = watch::channel(Duration::MAX);
        read.set_idle_timeout(timeout);
        let mut receive = Box::pin(async move {
            read.receive::<Vec<u8>, Vec<u8>, _, _>(Arc::new(RawSerializer), &Ls::default())
                .await
        });
        assert!(futures::poll!(receive.as_mut()).is_pending());
        tokio::time::advance(UdpIdleTimeout::default().0 * 2).await;
        assert!(futures::poll!(receive.as_mut()).is_pending());

        settings.send_replace(Duration::from_secs(2));
        peer.push(&[KEEPALIVE_DATAGRAM], Instant::now());
        assert!(futures::poll!(receive.as_mut()).is_pending());
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_idle(receive.await.unwrap_err());
    }

    #[tokio::test(start_paused = true)]
    async fn keepalives_prevent_idle_timeout() {
        let (mut client_read, _client_write, mut server_read, mut server_write) =
            connected_pair().await;
        let serializer = Arc::new(RawSerializer);
        let idle = tokio::spawn(async move {
            server_read
                .receive::<Vec<u8>, Vec<u8>, _, _>(Arc::new(RawSerializer), &Ls::default())
                .await
        });
        tokio::time::sleep(UdpIdleTimeout::default().0 * 3).await;
        server_write
            .send::<Vec<u8>, _, _, _>(vec![7], Arc::clone(&serializer), &Ls::default())
            .await
            .unwrap();
        let packet = client_read
            .receive::<_, Vec<u8>, _, _>(serializer, &Ls::default())
            .await
            .unwrap();
        assert_eq!(packet, vec![7]);
        assert!(!idle.is_finished());
    }

    #[tokio::test]
    async fn closing_either_side_disconnects_the_peer() {
        with_timeout(async {
            let (client_read, _client_write, mut server_read, _server_write) =
                connected_pair().await;
            drop(client_read);
            assert_disconnected(
                server_read
                    .receive::<Vec<u8>, Vec<u8>, _, _>(Arc::new(RawSerializer), &Ls::default())
                    .await
                    .unwrap_err(),
            );

            let (mut client_read, _client_write, server_read, _server_write) =
                connected_pair().await;
            drop(server_read);
            assert_disconnected(
                client_read
                    .receive::<Vec<u8>, Vec<u8>, _, _>(Arc::new(RawSerializer), &Ls::default())
                    .await
                    .unwrap_err(),
            );
        })
        .await;
    }

    #[tokio::test]
    async fn disconnected_peers_do_not_reopen_connections() {
        with_timeout(async {
            let (listener, mut client_read, mut client_write, server_read, _server_write) =
                connected().await;
            let client_addr = *listener.tasks.iter().next().unwrap().key();
            listener.handle_disconnection(client_addr);
            client_write
                .send::<Vec<u8>, _, _, _>(vec![7], Arc::new(RawSerializer), &Ls::default())
                .await
                .unwrap();
            assert_disconnected(
                client_read
                    .receive::<Vec<u8>, Vec<u8>, _, _>(Arc::new(RawSerializer), &Ls::default())
                    .await
                    .unwrap_err(),
            );
            assert!(listener.tasks.is_empty());
            drop(server_read);
        })
        .await;
    }

    #[tokio::test]
    async fn connect_resends_lost_probes() {
        with_timeout(async {
            let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let (client, ()) = tokio::join!(
                UdpClientStream::connect(server.local_addr().unwrap()),
                async {
                    let mut buf = [0; 16];
                    server.recv_from(&mut buf).await.unwrap();
                    let (len, client) = server.recv_from(&mut buf).await.unwrap();
                    assert_eq!(len, 0);
                    server.send_to(&[], client).await.unwrap();
                }
            );
            client.unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn connect_keeps_non_probe_answers() {
        with_timeout(async {
            let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let client = connect_to_raw(&server, &[DATA_DATAGRAM, 7]).await;
            let (mut read, _write) = client.into_split().await.unwrap();
            let packet = read
                .receive::<_, Vec<u8>, _, _>(Arc::new(RawSerializer), &Ls::default())
                .await
                .unwrap();
            assert_eq!(packet, vec![7]);
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn connect_times_out_without_an_answer() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let started = tokio::time::Instant::now();
        let err = UdpClientStream::connect(server.local_addr().unwrap())
            .await
            .err()
            .unwrap();
        assert_eq!(err.kind(), ErrorKind::TimedOut);
        assert_eq!(started.elapsed(), CONNECT_TIMEOUT);
    }

    #[cfg(any(feature = "client", feature = "server"))]
    #[test]
    fn idle_timeout_settings_are_per_app() {
        use bevy::prelude::{App, Update};

        let mut first = App::new();
        first
            .init_resource::<IdleTimeoutSettings>()
            .insert_resource(UdpIdleTimeout(Duration::MAX))
            .add_systems(Update, set_idle_timeout_system);
        let first_timeout = idle_timeout_receiver(first.world());
        assert_eq!(*first_timeout.borrow(), Duration::MAX);

        let mut second = App::new();
        second
            .init_resource::<IdleTimeoutSettings>()
            .add_systems(Update, set_idle_timeout_system);
        let second_timeout = idle_timeout_receiver(second.world());
        assert_eq!(*second_timeout.borrow(), UdpIdleTimeout::default().0);

        second.insert_resource(UdpIdleTimeout(Duration::from_secs(3)));
        second.update();
        first.update();
        assert_eq!(*first_timeout.borrow(), Duration::MAX);
        assert_eq!(*second_timeout.borrow(), Duration::from_secs(3));

        second.world_mut().remove_resource::<UdpIdleTimeout>();
        second.update();
        assert_eq!(*second_timeout.borrow(), UdpIdleTimeout::default().0);
        drop(first);
        assert_eq!(*second_timeout.borrow(), UdpIdleTimeout::default().0);
    }

    #[cfg(feature = "serializer_bitcode")]
    #[tokio::test]
    async fn bitcode_packets_include_empty_payloads() {
        use crate::serializer::SerializerAdapter;
        use crate::serializers::bitcode::BitcodeSerializer;

        async fn roundtrip<Packet>(packet: Packet)
        where
            Packet: bitcode::Encode
                + bitcode::DecodeOwned
                + Clone
                + Debug
                + PartialEq
                + Send
                + Sync
                + 'static,
        {
            let serializer = Arc::new(SerializerAdapter::ReadOnly(Arc::new(BitcodeSerializer)));
            let (mut client_read, mut client_write, mut server_read, mut server_write) =
                connected_pair().await;
            client_write
                .send::<Packet, _, _, _>(packet.clone(), Arc::clone(&serializer), &Ls::default())
                .await
                .unwrap();
            let received = server_read
                .receive::<_, Packet, _, _>(Arc::clone(&serializer), &Ls::default())
                .await
                .unwrap();
            assert_eq!(received, packet);
            server_write
                .send::<Packet, _, _, _>(received, Arc::clone(&serializer), &Ls::default())
                .await
                .unwrap();
            let received = client_read
                .receive::<_, Packet, _, _>(serializer, &Ls::default())
                .await
                .unwrap();
            assert_eq!(received, packet);
        }

        with_timeout(async {
            assert!(bitcode::encode(&()).is_empty());
            roundtrip(()).await;
            roundtrip((42_u32, "hello".to_owned())).await;
        })
        .await;
    }
}
