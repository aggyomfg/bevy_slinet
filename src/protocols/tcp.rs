//! TCP [`Protocol`] implementation based on [`tokio::net`]. You can enable it by adding `protocol_tcp` feature.
//!
//! Reuse the built-in framing with a custom socket factory:
//!
//! ```
//! use bevy_slinet::protocol::Protocol;
//! use bevy_slinet::protocols::tcp::{TcpNetworkListener, TcpNetworkStream};
//! use std::{io, net::SocketAddr};
//! use tokio::net::{TcpListener, TcpStream};
//!
//! struct TunedTcp;
//! #[async_trait::async_trait]
//! impl Protocol for TunedTcp {
//!     type Handle = ();
//!     type Listener = TcpNetworkListener;
//!     type ServerStream = TcpNetworkStream;
//!     type ClientStream = TcpNetworkStream;
//!
//!     async fn bind(addr: SocketAddr) -> io::Result<Self::Listener> {
//!         Ok(TcpNetworkListener::from_listener(TcpListener::bind(addr).await?)?
//!             .with_nodelay(true))
//!     }
//!
//!     async fn connect_to_server(addr: SocketAddr) -> io::Result<Self::ClientStream> {
//!         let socket = TcpStream::connect(addr).await?;
//!         socket.set_nodelay(true)?;
//!         TcpNetworkStream::from_stream(socket)
//!     }
//! }
//! ```
//!
//! Select `TunedTcp` as the config's `Protocol`. For other socket options, configure
//! a Tokio socket or convert a configured standard socket before wrapping it.

use std::io;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use crate::protocols::protocol::{
    ClientStream, FramedReader, FramedWriter, Listener, NetworkStream, Protocol, ReadStream,
    ServerStream, WriteStream,
};

/// TCP protocol.
pub struct TcpProtocol;

#[async_trait]
impl Protocol for TcpProtocol {
    type Handle = ();
    type Listener = TcpNetworkListener;
    type ServerStream = TcpNetworkStream;
    type ClientStream = TcpNetworkStream;

    async fn bind(addr: SocketAddr) -> io::Result<Self::Listener> {
        let listener = TcpListener::bind(addr).await?;
        TcpNetworkListener::from_listener(listener)
    }
}

/// A wrapped [TCP listener](std::net::TcpListener).
pub struct TcpNetworkListener {
    socket: TcpListener,
    address: SocketAddr,
    nodelay: Option<bool>,
}

impl TcpNetworkListener {
    /// Wraps an already bound listener, preserving its socket configuration.
    ///
    /// # Errors
    /// Returns an error if the local address cannot be queried.
    pub fn from_listener(socket: TcpListener) -> io::Result<Self> {
        Ok(Self {
            address: socket.local_addr()?,
            socket,
            nodelay: None,
        })
    }

    /// Sets `TCP_NODELAY` on subsequently accepted connections.
    /// Without this override, accepted sockets retain their inherited setting.
    #[must_use]
    pub const fn with_nodelay(mut self, nodelay: bool) -> Self {
        self.nodelay = Some(nodelay);
        self
    }
}

#[async_trait]
impl Listener for TcpNetworkListener {
    type Stream = TcpNetworkStream;

    async fn accept(&self) -> io::Result<TcpNetworkStream> {
        let (stream, _) = self.socket.accept().await?;
        if let Some(nodelay) = self.nodelay {
            stream.set_nodelay(nodelay)?;
        }
        TcpNetworkStream::from_stream(stream)
    }

    fn address(&self) -> SocketAddr {
        self.address
    }
}

/// A wrapped [TCP stream](std::net::TcpStream).
pub struct TcpNetworkStream(TcpStream, SocketAddr, SocketAddr);

impl TcpNetworkStream {
    /// Wraps a connected socket without changing its options.
    /// Configure `TCP_NODELAY`, keepalive or buffer sizes before wrapping it.
    ///
    /// # Errors
    /// Returns an error if the peer or local address cannot be queried.
    pub fn from_stream(stream: TcpStream) -> io::Result<Self> {
        let peer = stream.peer_addr()?;
        let local = stream.local_addr()?;
        Ok(Self(stream, peer, local))
    }

    /// Borrows the socket to inspect or configure options before splitting.
    /// Perform packet I/O through the framing halves to preserve message boundaries.
    #[must_use]
    pub const fn socket(&self) -> &TcpStream {
        &self.0
    }
}

#[async_trait]
impl NetworkStream for TcpNetworkStream {
    type Handle = ();
    type ReadHalf = FramedReader<OwnedReadHalf>;
    type WriteHalf = FramedWriter<OwnedWriteHalf>;

    async fn into_split(self) -> io::Result<(Self::ReadHalf, Self::WriteHalf)> {
        let (read, write) = self.0.into_split();
        Ok((FramedReader::new(read), FramedWriter::new(write)))
    }

    fn peer_addr(&self) -> SocketAddr {
        self.1
    }

    fn local_addr(&self) -> SocketAddr {
        self.2
    }

    fn transport(&self) -> Self::Handle {}
}

#[async_trait]
impl ReadStream for OwnedReadHalf {
    async fn read_exact(&mut self, buffer: &mut [u8]) -> io::Result<()> {
        AsyncReadExt::read_exact(self, buffer).await.map(|_| ())
    }
}

#[async_trait]
impl WriteStream for OwnedWriteHalf {
    async fn write_all(&mut self, buffer: &[u8]) -> io::Result<()> {
        AsyncWriteExt::write_all(self, buffer).await
    }
}

#[async_trait]
impl ClientStream for TcpNetworkStream {
    async fn connect(addr: SocketAddr) -> io::Result<Self>
    where
        Self: Sized,
    {
        Self::from_stream(TcpStream::connect(addr).await?)
    }
}

impl ServerStream for TcpNetworkStream {}
