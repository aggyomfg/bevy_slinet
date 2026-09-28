//! UDP protocol implementation based on [`tokio::net`]. You can enable it by adding `protocol_udp` feature.
//!
//! Every packet is sent as exactly one datagram, without a length prefix, so delivery is
//! unreliable and unordered: packets may be lost, duplicated or reordered, but a lost or
//! malformed datagram never affects other packets. Serialized packets must be non-empty and
//! fit into a single datagram ([`MAX_DATAGRAM_SIZE`] bytes); other packets are dropped.

use std::collections::VecDeque;
use std::fmt::Debug;
use std::io;
use std::io::ErrorKind;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::task::Poll;

use async_trait::async_trait;
use bevy::log;
use dashmap::DashMap;
use futures::future::poll_fn;
use futures::task::AtomicWaker;
use tokio::net::UdpSocket;

use crate::connection::MAX_PACKET_SIZE;
use crate::protocol::{
    ClientStream, Listener, NetworkStream, ReadStream, ReceiveError, ServerStream, WriteStream,
};
use crate::serializer::Serializer;
use crate::{PacketLengthSerializer, Protocol};

const BUFFER_SIZE: usize = u16::MAX as usize;
/// The largest UDP payload that can be sent over IPv4.
pub const MAX_DATAGRAM_SIZE: usize = 65_507;
/// Datagrams received from a peer beyond this many unread ones are dropped.
const MAX_QUEUED_DATAGRAMS: usize = 1024;

/// UDP protocol.
pub struct UdpProtocol;

#[async_trait]
impl Protocol for UdpProtocol {
    type Listener = UdpNetworkListener;
    type ServerStream = UdpServerStream;
    type ClientStream = UdpClientStream;

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
    datagrams: Mutex<VecDeque<Box<[u8]>>>,
}

#[derive(Clone, Default)]
struct UdpRead(Arc<Inner>);

impl UdpRead {
    fn push(&self, datagram: &[u8]) {
        // Empty datagrams are connection probes sent by `UdpClientStream::connect`.
        if datagram.is_empty() {
            return;
        }
        {
            let mut datagrams = self.0.datagrams.lock().unwrap();
            if datagrams.len() >= MAX_QUEUED_DATAGRAMS {
                return;
            }
            datagrams.push_back(datagram.into());
        }
        self.0.waker.wake();
    }

    async fn pop(&self) -> Box<[u8]> {
        poll_fn(|cx| {
            self.0.waker.register(cx.waker());
            match self.0.datagrams.lock().unwrap().pop_front() {
                Some(datagram) => Poll::Ready(datagram),
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
            let (len, address) = self.socket.recv_from(&mut buf).await?;
            let datagram = &buf[..len];
            if let Some(task) = self.tasks.get(&address) {
                task.push(datagram);
            } else {
                let new_task = UdpRead::default();
                new_task.push(datagram);
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
            UdpServerReadHalf(self.task.clone()),
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
pub struct UdpServerReadHalf(UdpRead);

#[async_trait]
impl ReadStream for UdpServerReadHalf {
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
        loop {
            let datagram = self.0.pop().await;
            if let Some(packet) = decode_datagram(&datagram, &*serializer) {
                return Ok(packet);
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
            Some(datagram) => self.write_all(&datagram).await,
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
        let read_socket = UdpSocket::from_std(std_socket)?;
        let write_socket = UdpSocket::from_std(std_socket2)?;
        let write = UdpClientWriteHalf {
            socket: write_socket,
        };
        let read = UdpClientReadHalf {
            socket: read_socket,
            buffer: vec![0; BUFFER_SIZE].into_boxed_slice(),
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
}

#[async_trait]
impl ReadStream for UdpClientReadHalf {
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
        loop {
            let len = self
                .socket
                .recv(&mut self.buffer)
                .await
                .map_err(ReceiveError::Io)?;
            if len == 0 {
                continue;
            }
            if let Some(packet) = decode_datagram(&self.buffer[..len], &*serializer) {
                return Ok(packet);
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
            Some(datagram) => self.write_all(&datagram).await,
            None => Ok(()),
        }
    }
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
) -> Option<ReceivingPacket>
where
    ReceivingPacket: Send + Sync + Debug + 'static,
    SendingPacket: Send + Sync + Debug + 'static,
    S: Serializer<ReceivingPacket, SendingPacket> + ?Sized,
{
    if datagram.len() > MAX_PACKET_SIZE.load(Ordering::Relaxed) {
        log::debug!(
            "Dropping a {}-byte datagram larger than MaxPacketSize",
            datagram.len()
        );
        return None;
    }
    match serializer.deserialize(datagram) {
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
    let datagram = serializer
        .serialize(packet)
        .expect("Error serializing packet");
    if datagram.is_empty() || datagram.len() > MAX_DATAGRAM_SIZE {
        log::warn!(
            "Dropping a {}-byte packet: UDP packets must be 1..={MAX_DATAGRAM_SIZE} bytes",
            datagram.len()
        );
        return None;
    }
    Some(datagram)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet_length_serializer::LittleEndian;

    /// Passes bytes through as-is; datagrams starting with `0xFF` are treated as malformed.
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

    #[tokio::test]
    async fn each_datagram_is_one_packet() {
        let serializer = Arc::new(RawSerializer);
        let ls = Ls::default();
        let listener = Arc::new(UdpProtocol::bind(([127, 0, 0, 1], 0).into()).await.unwrap());
        let client = UdpProtocol::connect_to_server(listener.address())
            .await
            .unwrap();
        let client_addr = client.local_addr();
        let server = listener.accept().await.unwrap();
        assert_eq!(server.peer_addr(), client_addr);
        let pump = Arc::clone(&listener);
        tokio::spawn(async move { while pump.accept().await.is_ok() {} });

        let (mut client_read, mut client_write) = client.into_split().await.unwrap();
        let (mut server_read, mut server_write) = server.into_split().await.unwrap();

        // The oversized packet is dropped on send, the 0xFF one on receive.
        for packet in [vec![1], vec![0; MAX_DATAGRAM_SIZE + 1], vec![2, 3]] {
            client_write
                .send::<Vec<u8>, _, _, _>(packet, Arc::clone(&serializer), &ls)
                .await
                .unwrap();
        }
        client_write.write_all(&[0xFF, 9]).await.unwrap();
        client_write
            .send::<Vec<u8>, _, _, _>(vec![4], Arc::clone(&serializer), &ls)
            .await
            .unwrap();

        for expected in [vec![1], vec![2, 3], vec![4]] {
            let packet = server_read
                .receive::<_, Vec<u8>, _, _>(Arc::clone(&serializer), &ls)
                .await
                .unwrap();
            assert_eq!(packet, expected);
        }

        server_write
            .send::<Vec<u8>, _, _, _>(vec![5], Arc::clone(&serializer), &ls)
            .await
            .unwrap();
        let packet = client_read
            .receive::<_, Vec<u8>, _, _>(Arc::clone(&serializer), &ls)
            .await
            .unwrap();
        assert_eq!(packet, vec![5]);
    }

    #[tokio::test]
    async fn first_datagram_is_not_lost() {
        let listener = UdpProtocol::bind(([127, 0, 0, 1], 0).into()).await.unwrap();
        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        peer.send_to(&[7], listener.address()).await.unwrap();

        let (mut read, _) = listener.accept().await.unwrap().into_split().await.unwrap();
        let packet = read
            .receive::<_, Vec<u8>, _, _>(Arc::new(RawSerializer), &Ls::default())
            .await
            .unwrap();
        assert_eq!(packet, vec![7]);
    }
}
