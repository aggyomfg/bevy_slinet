//! TCP [`Protocol`] implementation based on [`tokio::net`]. You can enable it by adding `protocol_tcp` feature.

use bevy::platform::time::Instant;
use std::fmt::Debug;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};

use crate::protocol::{
    ClientStream, FramedReader, Listener, NetworkStream, Protocol, ReadStream, ReceiveError,
    ServerStream, WriteStream,
};
use crate::serializer::Serializer;
use crate::PacketLengthSerializer;

/// TCP protocol.
pub struct TcpProtocol;

#[async_trait]
impl Protocol for TcpProtocol {
    type Listener = TcpNetworkListener;
    type ServerStream = TcpNetworkStream;
    type ClientStream = TcpNetworkStream;

    async fn bind(addr: SocketAddr) -> io::Result<Self::Listener> {
        Ok(TcpNetworkListener(TcpListener::bind(addr).await?))
    }
}

/// A wrapped [TCP listener](std::net::TcpListener).
pub struct TcpNetworkListener(TcpListener);

#[async_trait]
impl Listener for TcpNetworkListener {
    type Stream = TcpNetworkStream;

    async fn accept(&self) -> io::Result<TcpNetworkStream> {
        let (stream, _) = self.0.accept().await?;
        Ok(TcpNetworkStream(stream))
    }

    fn address(&self) -> SocketAddr {
        self.0.local_addr().unwrap()
    }
}

/// A wrapped [TCP stream](std::net::TcpStream).
pub struct TcpNetworkStream(TcpStream);

#[async_trait]
impl NetworkStream for TcpNetworkStream {
    type ReadHalf = OwnedReadHalf;
    type WriteHalf = OwnedWriteHalf;

    async fn into_split(self) -> io::Result<(Self::ReadHalf, Self::WriteHalf)> {
        Ok(self.0.into_split())
    }

    fn peer_addr(&self) -> SocketAddr {
        self.0.peer_addr().unwrap()
    }

    fn local_addr(&self) -> SocketAddr {
        self.0.local_addr().unwrap()
    }
}

#[async_trait]
impl ReadStream for OwnedReadHalf {
    async fn read_exact(&mut self, buffer: &mut [u8]) -> io::Result<()> {
        AsyncReadExt::read_exact(self, buffer).await.map(|_| ())
    }

    async fn receive_with_timestamp<ReceivingPacket, SendingPacket, S, LS>(
        &mut self,
        serializer: Arc<S>,
        length_serializer: &LS,
    ) -> Result<(ReceivingPacket, Instant), ReceiveError<S::DecodeError, LS>>
    where
        ReceivingPacket: Send + Sync + Debug + 'static,
        SendingPacket: Send + Sync + Debug + 'static,
        S: Serializer<ReceivingPacket, SendingPacket> + ?Sized,
        LS: PacketLengthSerializer,
    {
        FramedReader::new(self)
            .receive(serializer, length_serializer)
            .await
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
        Ok(TcpNetworkStream(TcpStream::connect(addr).await?))
    }
}

impl ServerStream for TcpNetworkStream {}
