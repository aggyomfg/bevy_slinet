//! UDP protocol implementation based on [`tokio::net`]. You can enable it by adding `protocol_udp` feature.
//!
//! Every packet is sent as exactly one datagram, without a length prefix. An empty datagram
//! is a connection probe; data datagrams contain a `1` byte followed by the serialized payload,
//! which may be empty. The payload must fit in [`MAX_DATAGRAM_SIZE`] minus one byte (65,506
//! bytes); larger packets are dropped. `MaxPacketSize` limits the payload, excluding the tag.
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
//! Both peers send a `2` keep-alive datagram every [`KEEPALIVE_INTERVAL`] and close the
//! connection after [`UdpIdleTimeout`] without any datagrams from the other side.

use std::collections::VecDeque;
use std::fmt::Debug;
use std::io;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use async_trait::async_trait;
use bevy::log;
use bevy::platform::time::Instant;
use bevy::prelude::Resource;
use dashmap::DashMap;
use futures::future::poll_fn;
use futures::task::AtomicWaker;
use tokio::net::UdpSocket;
use tokio::sync::oneshot;
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
/// How often each peer sends a keep-alive datagram.
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(1);
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

/// Sends keep-alive datagrams until the returned sender is dropped.
fn spawn_keepalive(socket: Arc<UdpSocket>, peer_addr: Option<SocketAddr>) -> oneshot::Sender<()> {
    let (stop, mut stopped) = oneshot::channel();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(KEEPALIVE_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                _ = &mut stopped => break,
                _ = interval.tick() => {
                    // Errors are reported to the read half or end in an idle timeout.
                    let _ = match peer_addr {
                        Some(addr) => socket.send_to(&[KEEPALIVE_DATAGRAM], addr).await,
                        None => socket.send(&[KEEPALIVE_DATAGRAM]).await,
                    };
                }
            }
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

#[derive(Default)]
struct Inner {
    waker: AtomicWaker,
    queue: Mutex<Queue>,
}

#[derive(Default)]
struct Queue {
    datagrams: VecDeque<(Box<[u8]>, Instant)>,
    bytes: usize,
}

#[derive(Clone, Default)]
struct UdpRead(Arc<Inner>);

impl UdpRead {
    fn push(&self, datagram: &[u8], received_at: Instant) {
        // Empty datagrams are connection probes sent by `UdpClientStream::connect`.
        if datagram.is_empty() {
            return;
        }
        {
            let mut queue = self.0.queue.lock().unwrap();
            if queue.datagrams.len() >= MAX_QUEUED_DATAGRAMS
                || queue.bytes + datagram.len() > MAX_QUEUED_BYTES
            {
                return;
            }
            queue.bytes += datagram.len();
            queue.datagrams.push_back((datagram.into(), received_at));
        }
        self.0.waker.wake();
    }

    async fn pop(&self) -> (Box<[u8]>, Instant) {
        poll_fn(|cx| {
            self.0.waker.register(cx.waker());
            let mut queue = self.0.queue.lock().unwrap();
            match queue.datagrams.pop_front() {
                Some(datagram) => {
                    queue.bytes -= datagram.0.len();
                    Poll::Ready(datagram)
                }
                None => Poll::Pending,
            }
        })
        .await
    }
}

/// A UDP listener.
pub struct UdpNetworkListener {
    socket: Arc<UdpSocket>,
    tasks: DashMap<SocketAddr, UdpRead>,
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
            if let Some(task) = self.tasks.get(&address) {
                task.push(datagram, received_at);
            } else if matches!(datagram.first(), None | Some(&DATA_DATAGRAM)) {
                let new_task = UdpRead::default();
                new_task.push(datagram, received_at);
                self.tasks.insert(address, new_task.clone());
                return Ok(UdpServerStream {
                    task: new_task,
                    peer_addr: address,
                    socket: Arc::clone(&self.socket),
                });
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

/// A UDP server stream that contains cached bytes and a task waker.
pub struct UdpServerStream {
    task: UdpRead,
    peer_addr: SocketAddr,
    socket: Arc<UdpSocket>,
}

#[async_trait]
impl NetworkStream for UdpServerStream {
    type ReadHalf = UdpServerReadHalf;
    type WriteHalf = UdpServerWriteHalf;

    async fn into_split(self) -> io::Result<(Self::ReadHalf, Self::WriteHalf)> {
        let peer_addr = self.peer_addr();
        Ok((
            UdpServerReadHalf {
                task: self.task.clone(),
                idle_timeout: tokio::sync::watch::channel(UdpIdleTimeout::default().0).1,
                _keepalive: spawn_keepalive(Arc::clone(&self.socket), Some(peer_addr)),
            },
            UdpServerWriteHalf {
                peer_addr,
                socket: self.socket,
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

/// The read half of [`UdpServerStream`].
pub struct UdpServerReadHalf {
    task: UdpRead,
    idle_timeout: tokio::sync::watch::Receiver<Duration>,
    _keepalive: oneshot::Sender<()>,
}

#[async_trait]
impl ReadStream for UdpServerReadHalf {
    fn set_idle_timeout(&mut self, timeout: tokio::sync::watch::Receiver<Duration>) {
        self.idle_timeout = timeout;
    }

    async fn read_exact(&mut self, _buffer: &mut [u8]) -> io::Result<()> {
        Err(datagram_only_error())
    }

    async fn receive<ReceivingPacket, SendingPacket, S, LS>(
        &mut self,
        serializer: Arc<S>,
        _length_serializer: &LS,
    ) -> Result<ReceivingPacket, ReceiveError<S::DecodeError, LS>>
    where
        ReceivingPacket: Send + Sync + Debug + 'static,
        SendingPacket: Send + Sync + Debug + 'static,
        S: Serializer<ReceivingPacket, SendingPacket> + ?Sized,
        LS: PacketLengthSerializer,
    {
        self.receive_with_timestamp(serializer, _length_serializer)
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
            let (datagram, received_at) = tokio::time::timeout(idle_timeout, self.task.pop())
                .await
                .map_err(|_| idle_error())?;
            if let Some(packet) = decode_datagram(
                &datagram,
                &*serializer,
                MAX_PACKET_SIZE.load(Ordering::Relaxed),
            ) {
                return Ok((packet, received_at));
            }
        }
    }
}

/// A write half of [`UdpServerStream`];
pub struct UdpServerWriteHalf {
    peer_addr: SocketAddr,
    socket: Arc<UdpSocket>,
}

#[async_trait]
impl WriteStream for UdpServerWriteHalf {
    async fn write_all(&mut self, buffer: &[u8]) -> std::io::Result<()> {
        self.socket
            .send_to(buffer, self.peer_addr)
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
    socket: UdpSocket,
    peer_addr: SocketAddr,
}

#[async_trait]
impl NetworkStream for UdpClientStream {
    type ReadHalf = UdpClientReadHalf;
    type WriteHalf = UdpClientWriteHalf;

    async fn into_split(mut self) -> io::Result<(Self::ReadHalf, Self::WriteHalf)> {
        let std_socket = self.socket.into_std()?;
        let std_socket2 = std_socket.try_clone()?;
        let keepalive_socket = Arc::new(UdpSocket::from_std(std_socket.try_clone()?)?);
        let read_socket = UdpSocket::from_std(std_socket)?;
        let write_socket = UdpSocket::from_std(std_socket2)?;
        let write = UdpClientWriteHalf {
            socket: write_socket,
        };
        let read = UdpClientReadHalf {
            socket: read_socket,
            buffer: vec![0; BUFFER_SIZE].into_boxed_slice(),
            idle_timeout: tokio::sync::watch::channel(UdpIdleTimeout::default().0).1,
            _keepalive: spawn_keepalive(keepalive_socket, None),
        };
        Ok((read, write))
    }

    fn peer_addr(&self) -> SocketAddr {
        self.peer_addr // self.0.peer_addr().unwrap(). Tokio added it in https://github.com/tokio-rs/tokio/pull/4362 and then reverted in https://github.com/tokio-rs/tokio/pull/4392
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

        // TODO remove this
        let std_socket = socket.into_std().unwrap();
        let peer_addr = std_socket.peer_addr().unwrap();
        let socket = UdpSocket::from_std(std_socket).unwrap();

        // socket.connect and socket.send is not enough to handle ConnectionRefused, but 2 sends is
        socket.send(&[]).await?;
        socket.send(&[]).await?;
        Ok(UdpClientStream { socket, peer_addr })
    }
}

/// A read half of [`UdpClientStream`].
pub struct UdpClientReadHalf {
    socket: UdpSocket,
    buffer: Box<[u8]>,
    idle_timeout: tokio::sync::watch::Receiver<Duration>,
    _keepalive: oneshot::Sender<()>,
}

#[async_trait]
impl ReadStream for UdpClientReadHalf {
    fn set_idle_timeout(&mut self, timeout: tokio::sync::watch::Receiver<Duration>) {
        self.idle_timeout = timeout;
    }

    async fn read_exact(&mut self, _buffer: &mut [u8]) -> io::Result<()> {
        Err(datagram_only_error())
    }

    async fn receive<ReceivingPacket, SendingPacket, S, LS>(
        &mut self,
        serializer: Arc<S>,
        _length_serializer: &LS,
    ) -> Result<ReceivingPacket, ReceiveError<S::DecodeError, LS>>
    where
        ReceivingPacket: Send + Sync + Debug + 'static,
        SendingPacket: Send + Sync + Debug + 'static,
        S: Serializer<ReceivingPacket, SendingPacket> + ?Sized,
        LS: PacketLengthSerializer,
    {
        self.receive_with_timestamp(serializer, _length_serializer)
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
            let len = tokio::time::timeout(idle_timeout, self.socket.recv(&mut self.buffer))
                .await
                .map_err(|_| idle_error())?
                .map_err(ReceiveError::Io)?;
            let received_at = Instant::now();
            if let Some(packet) = decode_datagram(
                &self.buffer[..len],
                &*serializer,
                MAX_PACKET_SIZE.load(Ordering::Relaxed),
            ) {
                return Ok((packet, received_at));
            }
        }
    }
}

/// A write half of [`UdpClientStream`].
pub struct UdpClientWriteHalf {
    socket: UdpSocket,
}

#[async_trait]
impl WriteStream for UdpClientWriteHalf {
    async fn write_all(&mut self, buffer: &[u8]) -> std::io::Result<()> {
        self.socket
            .send(buffer)
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

/// Returns `None` for datagrams that should be dropped without closing the connection.
fn decode_datagram<ReceivingPacket, SendingPacket, S>(
    datagram: &[u8],
    serializer: &S,
    max_packet_size: usize,
) -> Option<ReceivingPacket>
where
    ReceivingPacket: Send + Sync + Debug + 'static,
    SendingPacket: Send + Sync + Debug + 'static,
    S: Serializer<ReceivingPacket, SendingPacket> + ?Sized,
{
    let (&tag, payload) = datagram.split_first()?;
    if tag != DATA_DATAGRAM || datagram.len() > MAX_DATAGRAM_SIZE {
        return None;
    }
    if payload.len() > max_packet_size {
        log::debug!(
            "Dropping a {}-byte payload larger than MaxPacketSize",
            payload.len()
        );
        return None;
    }
    match serializer.deserialize(payload) {
        Ok(packet) => Some(packet),
        Err(err) => {
            log::debug!("Dropping a malformed datagram: {err}");
            None
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

    async fn connected_pair() -> (
        UdpClientReadHalf,
        UdpClientWriteHalf,
        UdpServerReadHalf,
        UdpServerWriteHalf,
    ) {
        let listener = Arc::new(UdpProtocol::bind(([127, 0, 0, 1], 0).into()).await.unwrap());
        let client = UdpProtocol::connect_to_server(listener.address())
            .await
            .unwrap();
        let client_addr = client.local_addr();
        let server = listener.accept().await.unwrap();
        assert_eq!(server.peer_addr(), client_addr);
        let pump = Arc::clone(&listener);
        tokio::spawn(async move { while pump.accept().await.is_ok() {} });

        let (client_read, client_write) = client.into_split().await.unwrap();
        let (server_read, server_write) = server.into_split().await.unwrap();
        (client_read, client_write, server_read, server_write)
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
    async fn first_datagram_is_not_lost() {
        with_timeout(async {
            let listener = UdpProtocol::bind(([127, 0, 0, 1], 0).into()).await.unwrap();
            let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let before_send = Instant::now();
            peer.send_to(&[DATA_DATAGRAM, 7], listener.address())
                .await
                .unwrap();

            let stream = listener.accept().await.unwrap();
            let after_accept = Instant::now();
            let (mut read, _) = stream.into_split().await.unwrap();
            let (packet, received_at) = read
                .receive_with_timestamp::<_, Vec<u8>, _, _>(Arc::new(RawSerializer), &Ls::default())
                .await
                .unwrap();
            assert_eq!(packet, vec![7]);
            assert!(before_send <= received_at && received_at <= after_accept);
        })
        .await;
    }

    #[tokio::test]
    async fn queued_packets_preserve_their_timestamps() {
        let task = UdpRead::default();
        task.push(&[KEEPALIVE_DATAGRAM], Instant::now());
        task.push(&[DATA_DATAGRAM, 0xFF], Instant::now());
        let first = Instant::now();
        task.push(&[DATA_DATAGRAM, 7], first);
        let second = Instant::now();
        task.push(&[DATA_DATAGRAM], second);
        let mut read = UdpServerReadHalf {
            task,
            idle_timeout: tokio::sync::watch::channel(UdpIdleTimeout::default().0).1,
            _keepalive: oneshot::channel().0,
        };
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
            for datagram in [&[0][..], &[0xAB, 7], &[KEEPALIVE_DATAGRAM]] {
                garbage.send_to(datagram, listener.address()).await.unwrap();
            }
            peer.send_to(&[], listener.address()).await.unwrap();

            let stream = listener.accept().await.unwrap();
            assert_eq!(stream.peer_addr(), peer.local_addr().unwrap());
            assert!(!listener.tasks.contains_key(&garbage.local_addr().unwrap()));
        })
        .await;
    }

    #[test]
    fn datagram_size_limits() {
        let payload = vec![7; MAX_DATAGRAM_SIZE - 1];
        let datagram = encode_datagram(payload.clone(), &RawSerializer).unwrap();
        assert_eq!(datagram.len(), MAX_DATAGRAM_SIZE);
        assert_eq!(datagram[0], DATA_DATAGRAM);
        assert_eq!(
            decode_datagram(&datagram, &RawSerializer, payload.len()),
            Some(payload.clone())
        );
        assert_eq!(
            decode_datagram(&datagram, &RawSerializer, payload.len() - 1),
            None
        );
        assert!(encode_datagram(vec![7; MAX_DATAGRAM_SIZE], &RawSerializer).is_none());
        assert_eq!(
            decode_datagram(
                &vec![DATA_DATAGRAM; MAX_DATAGRAM_SIZE + 1],
                &RawSerializer,
                usize::MAX
            ),
            None
        );
        assert_eq!(
            encode_datagram(vec![], &RawSerializer),
            Some(vec![DATA_DATAGRAM])
        );
        assert_eq!(
            decode_datagram(&[DATA_DATAGRAM], &RawSerializer, 0),
            Some(vec![])
        );
        assert_eq!(
            decode_datagram(&[DATA_DATAGRAM, 7], &RawSerializer, 0),
            None
        );
        assert_eq!(decode_datagram(&[], &RawSerializer, 0), None);
        assert_eq!(
            decode_datagram(&[KEEPALIVE_DATAGRAM], &RawSerializer, usize::MAX),
            None
        );
        assert_eq!(
            decode_datagram(&[0xAB, 7], &RawSerializer, usize::MAX),
            None
        );
    }

    #[tokio::test]
    async fn send_errors_drop_only_the_packet() {
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let mut write = UdpServerWriteHalf {
            peer_addr: (Ipv6Addr::LOCALHOST, 1).into(),
            socket,
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
            .filter_map(|datagram| decode_datagram(datagram, &RawSerializer, 2))
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
            let queue = UdpRead::default();
            queue.push(&[], Instant::now());
            assert!(queue.0.queue.lock().unwrap().datagrams.is_empty());
            for _ in 0..MAX_QUEUED_DATAGRAMS {
                queue.push(&[DATA_DATAGRAM, 7], Instant::now());
            }
            queue.push(&[DATA_DATAGRAM, 8], Instant::now());
            assert_eq!(
                queue.0.queue.lock().unwrap().datagrams.len(),
                MAX_QUEUED_DATAGRAMS
            );
            assert_eq!(&*queue.pop().await.0, &[DATA_DATAGRAM, 7]);
            queue.push(&[DATA_DATAGRAM, 9], Instant::now());
            for _ in 1..MAX_QUEUED_DATAGRAMS {
                assert_eq!(&*queue.pop().await.0, &[DATA_DATAGRAM, 7]);
            }
            assert_eq!(&*queue.pop().await.0, &[DATA_DATAGRAM, 9]);
            assert_eq!(queue.0.queue.lock().unwrap().bytes, 0);
            let mut pending = Box::pin(queue.pop());
            assert!(futures::poll!(pending.as_mut()).is_pending());
            queue.push(&[DATA_DATAGRAM], Instant::now());
            assert_eq!(&*pending.await.0, &[DATA_DATAGRAM]);
        })
        .await;
    }

    #[tokio::test]
    async fn queue_byte_budget_drops_only_excess_datagrams() {
        with_timeout(async {
            let queue = UdpRead::default();
            let big = vec![DATA_DATAGRAM; MAX_DATAGRAM_SIZE];
            let fits = MAX_QUEUED_BYTES / MAX_DATAGRAM_SIZE;
            for _ in 0..fits + 1 {
                queue.push(&big, Instant::now());
            }
            let small_fits = MAX_QUEUED_BYTES - fits * MAX_DATAGRAM_SIZE;
            queue.push(&vec![DATA_DATAGRAM; small_fits], Instant::now());
            queue.push(&[DATA_DATAGRAM], Instant::now());
            {
                let queued = queue.0.queue.lock().unwrap();
                assert_eq!(queued.datagrams.len(), fits + 1);
                assert_eq!(queued.bytes, MAX_QUEUED_BYTES);
            }
            assert_eq!(queue.pop().await.0.len(), MAX_DATAGRAM_SIZE);
            queue.push(&big, Instant::now());
            assert_eq!(queue.0.queue.lock().unwrap().bytes, MAX_QUEUED_BYTES);
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
            let listener = UdpProtocol::bind(([127, 0, 0, 1], 0).into()).await.unwrap();
            let client_peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            client_peer.send_to(&[], listener.address()).await.unwrap();
            let _server = listener.accept().await.unwrap().into_split().await.unwrap();
            assert_eq!(next_non_probe(&client_peer).await, [KEEPALIVE_DATAGRAM]);

            let server_peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let client = UdpClientStream::connect(server_peer.local_addr().unwrap())
                .await
                .unwrap();
            let _client = client.into_split().await.unwrap();
            assert_eq!(next_non_probe(&server_peer).await, [KEEPALIVE_DATAGRAM]);
        })
        .await;
    }

    #[tokio::test(start_paused = true)]
    async fn keepalives_stop_with_the_read_half() {
        let listener = UdpProtocol::bind(([127, 0, 0, 1], 0).into()).await.unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        peer.send_to(&[], listener.address()).await.unwrap();
        let (read, _write) = listener.accept().await.unwrap().into_split().await.unwrap();
        drop(read);
        tokio::time::sleep(KEEPALIVE_INTERVAL * 5).await;
        let mut buf = [0; 16];
        let err = peer.try_recv_from(&mut buf).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::WouldBlock);
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

        let client = UdpClientStream::connect(peer.local_addr().unwrap())
            .await
            .unwrap();
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
        let client = UdpClientStream::connect(peer.local_addr().unwrap())
            .await
            .unwrap();
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
        let task = UdpRead::default();
        let (settings, timeout) = tokio::sync::watch::channel(Duration::MAX);
        let mut read = UdpServerReadHalf {
            task: task.clone(),
            idle_timeout: timeout,
            _keepalive: oneshot::channel().0,
        };
        let mut receive = Box::pin(async move {
            read.receive::<Vec<u8>, Vec<u8>, _, _>(Arc::new(RawSerializer), &Ls::default())
                .await
        });
        assert!(futures::poll!(receive.as_mut()).is_pending());
        tokio::time::advance(UdpIdleTimeout::default().0 * 2).await;
        assert!(futures::poll!(receive.as_mut()).is_pending());

        settings.send_replace(Duration::from_secs(2));
        task.push(&[KEEPALIVE_DATAGRAM], Instant::now());
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
