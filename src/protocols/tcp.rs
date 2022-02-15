//! TCP protocol implementation based on [`std::net`]. You can enable it by adding `tcp` feature.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::protocol::{ClientStream, Listener, NetworkStream, Protocol, ServerStream};

/// TCP protocol.
pub struct TcpProtocol;

impl Protocol for TcpProtocol {
    type Listener = TcpNetworkListener;
    type ServerStream = TcpNetworkStream;
    type ClientStream = TcpNetworkStream;

    fn bind<A>(addr: A) -> std::io::Result<Self::Listener>
    where
        A: ToSocketAddrs,
    {
        Ok(TcpNetworkListener(TcpListener::bind(addr)?))
    }
}

/// A wrapped [TCP listener](std::net::TcpListener).
pub struct TcpNetworkListener(TcpListener);

impl Listener<TcpNetworkStream> for TcpNetworkListener {
    fn accept(&self) -> std::io::Result<(TcpNetworkStream, SocketAddr)> {
        let (stream, addr) = self.0.accept()?;
        Ok((TcpNetworkStream(stream), addr))
    }

    fn set_nonblocking(&self) {
        self.0.set_nonblocking(true).unwrap()
    }

    fn address(&self) -> SocketAddr {
        self.0.local_addr().unwrap()
    }
}

/// A wrapped [TCP stream](std::net::TcpStream).
pub struct TcpNetworkStream(TcpStream);

impl Read for TcpNetworkStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for TcpNetworkStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl NetworkStream for TcpNetworkStream {
    fn set_nonblocking(&self) {
        self.0.set_nonblocking(true).unwrap()
    }

    fn try_peek_exact(&self, buffer: &mut [u8]) -> std::io::Result<()> {
        if matches!(self.0.peek(buffer), Ok(n) if n == buffer.len()) {
            Ok(())
        } else {
            Err(std::io::Error::from(ErrorKind::WouldBlock))
        }
    }

    fn read_exact(&mut self, buffer: &mut [u8]) -> std::io::Result<()> {
        self.0.try_clone()?.read_exact(buffer)
    }

    fn write_all(&mut self, buffer: &[u8]) -> std::io::Result<()> {
        self.0.try_clone()?.write_all(buffer)
    }

    fn local_addr(&self) -> SocketAddr {
        self.0.local_addr().unwrap()
    }

    fn peer_addr(&self) -> SocketAddr {
        self.0.peer_addr().unwrap()
    }
}

impl ClientStream for TcpNetworkStream {
    fn connect<A>(addr: A) -> std::io::Result<Self>
    where
        Self: Sized,
        A: ToSocketAddrs,
    {
        Ok(TcpNetworkStream(TcpStream::connect_timeout(
            &addr
                .to_socket_addrs()
                .expect("Invalid address")
                .next()
                .expect("Invalid address"),
            Duration::from_secs(10),
        )?))
    }
}

impl ServerStream for TcpNetworkStream {}
