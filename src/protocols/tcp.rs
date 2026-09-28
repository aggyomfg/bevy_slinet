//! TCP [`Protocol`] implementation based on [`tokio::net`]. You can enable it by adding `protocol_tcp` feature.

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
        let address = listener.local_addr()?;
        Ok(TcpNetworkListener(listener, address))
    }
}

/// A wrapped [TCP listener](std::net::TcpListener).
pub struct TcpNetworkListener(TcpListener, SocketAddr);

#[async_trait]
impl Listener for TcpNetworkListener {
    type Stream = TcpNetworkStream;

    async fn accept(&self) -> io::Result<TcpNetworkStream> {
        let (stream, _) = self.0.accept().await?;
        TcpNetworkStream::new(stream)
    }

    fn address(&self) -> SocketAddr {
        self.1
    }
}

/// A wrapped [TCP stream](std::net::TcpStream).
pub struct TcpNetworkStream(TcpStream, SocketAddr, SocketAddr);

impl TcpNetworkStream {
    fn new(stream: TcpStream) -> io::Result<Self> {
        let peer = stream.peer_addr()?;
        let local = stream.local_addr()?;
        Ok(Self(stream, peer, local))
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
        Self::new(TcpStream::connect(addr).await?)
    }
}

impl ServerStream for TcpNetworkStream {}
