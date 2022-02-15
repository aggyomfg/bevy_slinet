//! UDP protocol implementation based on [`std::net`]. You can enable it by adding `udp` feature.
// The implementation is very poor and should be rewritten ASAP.

use std::io::{Error, ErrorKind, Write};
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::Arc;

use dashmap::DashMap;

use crate::protocol::{ClientStream, Listener, NetworkStream, Protocol, ServerStream};

type ListenerCache = Arc<DashMap<SocketAddr, Vec<u8>>>;

/// UDP protocol.
pub struct UdpProtocol;

impl Protocol for UdpProtocol {
    type Listener = UdpNetworkListener;
    type ServerStream = ListenerReader;
    type ClientStream = WrappedUdpSocket;

    fn bind<A>(addr: A) -> std::io::Result<Self::Listener>
    where
        A: ToSocketAddrs,
    {
        Ok(UdpNetworkListener {
            socket: UdpSocket::bind(addr)?,
            cache: Arc::new(DashMap::new()),
        })
    }
}

/// A UDP listener. Packets that have length greater than max allowed by UDP may fail.
/// It implements a "cache" that is used to disambiguate packets from different addresses
/// because UDP doesn't have a stable connection.
pub struct UdpNetworkListener {
    socket: UdpSocket,
    cache: ListenerCache,
}

impl Listener<ListenerReader> for UdpNetworkListener {
    fn accept(&self) -> std::io::Result<(ListenerReader, SocketAddr)> {
        // Let's hope that the other side didn't send more data than MAX_UDP_PACKET_SIZE.
        // There should be a better solution (a pr is welcome), but anyway, UDP packets can
        // be lost and the app should be okay with that, or use alternative protocols like TCP
        let mut buf = [0; MAX_UDP_PACKET_SIZE];
        let (amt, addr) = self.socket.recv_from(&mut buf)?;
        if let Some(mut cache) = self.cache.get_mut(&addr) {
            // add the received data to cache and ListenerReader will read from this cache
            cache.extend_from_slice(&buf[..amt]);
            Err(std::io::Error::new(
                ErrorKind::AlreadyExists,
                "Data received from an existing connection",
            ))
        } else if amt == 0 {
            // an empty packet is used to acknowledge server that the client has connected
            self.cache.insert(addr, Vec::new());
            Ok((
                ListenerReader {
                    socket: self.socket.try_clone().unwrap(),
                    addr,
                    cache: Arc::clone(&self.cache),
                },
                addr,
            ))
        } else {
            Err(std::io::Error::new(
                ErrorKind::InvalidData,
                "Received something other than an empty handshake packet",
            ))
        }
    }

    fn set_nonblocking(&self) {
        self.socket.set_nonblocking(true).unwrap()
    }

    fn address(&self) -> SocketAddr {
        self.socket.local_addr().unwrap()
    }
}

/// A wrapped [UDP socket](std::net::UdpSocket).
pub struct WrappedUdpSocket {
    socket: UdpSocket,
    cache: Vec<u8>,
}

/// A maximum number of bytes sent per one UDP packets, including serialized packet length.
pub const MAX_UDP_PACKET_SIZE: usize = 65507;

impl ClientStream for WrappedUdpSocket {
    fn connect<A>(addr: A) -> std::io::Result<Self>
    where
        A: ToSocketAddrs,
    {
        let socket = UdpSocket::bind("127.0.0.1:0")?;
        socket.connect(&addr)?;
        socket.send(&[])?;
        Ok(WrappedUdpSocket {
            socket,
            cache: Vec::new(),
        })
    }
}

impl ServerStream for ListenerReader {}

impl NetworkStream for ListenerReader {
    fn set_nonblocking(&self) {
        self.socket.set_nonblocking(true).unwrap()
    }

    fn try_peek_exact(&self, buffer: &mut [u8]) -> std::io::Result<()> {
        if let Some(cache) = self.cache.get(&self.addr) {
            if cache.len() < buffer.len() {
                Err(std::io::Error::from(ErrorKind::WouldBlock))
            } else {
                buffer.clone_from_slice(&cache[..buffer.len()]);
                Ok(())
            }
        } else {
            Err(std::io::Error::new(
                ErrorKind::NotFound,
                "Connection to this socket not found",
            ))
        }
    }

    fn read_exact(&mut self, buffer: &mut [u8]) -> std::io::Result<()> {
        if let Some(mut cache) = self.cache.get_mut(&self.addr) {
            if cache.is_empty() {
                Err(std::io::Error::from(ErrorKind::WouldBlock))
            } else {
                let read = cache.len().min(buffer.len());
                if read == buffer.len() {
                    buffer.clone_from_slice(cache.drain(..read).as_slice());
                    Ok(())
                } else {
                    Err(Error::from(ErrorKind::BrokenPipe))
                }
            }
        } else {
            Err(std::io::Error::new(
                ErrorKind::NotFound,
                "Connection to this socket not found",
            ))
        }
    }

    fn write_all(&mut self, buffer: &[u8]) -> std::io::Result<()> {
        self.socket.send_to(buffer, self.addr).and_then(|i| {
            if i == buffer.len() {
                Ok(())
            } else {
                Err(Error::from(ErrorKind::BrokenPipe))
            }
        })
    }

    fn peer_addr(&self) -> SocketAddr {
        self.addr
    }

    fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr().unwrap()
    }
}

impl NetworkStream for WrappedUdpSocket {
    fn set_nonblocking(&self) {
        self.socket.set_nonblocking(true).unwrap()
    }

    fn try_peek_exact(&self, buffer: &mut [u8]) -> std::io::Result<()> {
        if matches!(self.socket.peek(buffer), Ok(n) if n == buffer.len()) {
            Ok(())
        } else {
            Err(std::io::Error::from(ErrorKind::WouldBlock))
        }
    }

    fn read_exact(&mut self, buffer: &mut [u8]) -> std::io::Result<()> {
        let mut size = buffer.len();
        if size > self.cache.len() {
            // If there's not enough data in the cache, try to recv from UdpSocket.
            let mut buf = vec![0; MAX_UDP_PACKET_SIZE];
            let len = self.socket.recv(&mut buf)?;
            self.cache.write_all(&buf[..len])?;
            size = size.min(self.cache.len());
        }
        if size == 0 {
            Err(std::io::Error::from(ErrorKind::WouldBlock))
        } else {
            let read = self.cache.len().min(buffer.len());
            if read == buffer.len() {
                buffer.clone_from_slice(self.cache.drain(..read).as_slice());
                Ok(())
            } else {
                Err(Error::from(ErrorKind::BrokenPipe))
            }
        }
    }

    fn write_all(&mut self, buffer: &[u8]) -> std::io::Result<()> {
        self.socket.send(buffer).and_then(|i| {
            if i == buffer.len() {
                Ok(())
            } else {
                Err(Error::from(ErrorKind::BrokenPipe))
            }
        })
    }

    fn peer_addr(&self) -> SocketAddr {
        self.socket.peer_addr().unwrap()
    }

    fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr().unwrap()
    }
}

/// ListenerReader reads data from the listener's cache
pub struct ListenerReader {
    socket: UdpSocket,
    addr: SocketAddr,
    cache: ListenerCache,
}
