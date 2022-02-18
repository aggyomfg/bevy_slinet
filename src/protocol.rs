//! Implement [`Protocol`] to create your own protocol implementation and use
//! it in [`ServerConfig`](crate::ServerConfig) or [`ClientConfig`](crate::ClientConfig).
//!
//! Built-in protocols are listed in the [`protocols`](crate::protocols) module.

use io::Write;
use std::fmt::{Debug, Formatter};
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use tokio::net::ToSocketAddrs;

use crate::connection::MAX_PACKET_SIZE;
use crate::packet_length_serializer::PacketLengthDeserializationError;
use crate::{PacketLengthSerializer, Serializer};

/// In order to simplify protocol switching and implementation, there is a [`Protocol`] trait.
/// Implement it or use built-in [`protocols`](crate::protocols).
#[async_trait]
pub trait Protocol: Send + Sync + 'static {
    /// A server-side listener type.
    type Listener: Listener<Self::ServerStream>;
    /// A server-side network stream. It can be different from [`Self::ClientStream`]
    type ServerStream: ServerStream;
    /// A client-side network stream. It can be different from [`Self::ServerStream`]
    type ClientStream: ClientStream;

    /// Creates and runs a [Listener](Self::Listener). Most of the time
    /// you want to use [create_listener](Self::create_listener) instead
    /// of this.
    async fn bind<A>(addr: A) -> io::Result<Self::Listener>
    where
        A: ToSocketAddrs + Send;

    /// Connect to the server at specified address.
    async fn connect_to_server<A>(addr: A) -> io::Result<Self::ClientStream>
    where
        A: ToSocketAddrs + Send,
    {
        let stream = Self::ClientStream::connect(addr).await?;
        log::debug!("Connected to a server at {:?}", stream.peer_addr());
        Ok(stream)
    }
}

/// A listener that accepts connections from clients.
#[async_trait]
pub trait Listener<S>: Send + Sync + 'static
where
    S: ServerStream,
{
    /// Returns a [ServerStream](ServerStream) when a client wants to connect.
    /// Runs in an endless loop.
    async fn accept(&self) -> io::Result<S>;

    /// Get the local address that this listeners listens at.
    fn address(&self) -> SocketAddr;
}

/// A [NetworkStream](NetworkStream) that can be used client-side.
#[async_trait]
pub trait ClientStream: NetworkStream {
    /// Connects to a server.
    async fn connect<A>(addr: A) -> io::Result<Self>
    where
        Self: Sized,
        A: ToSocketAddrs + Send;
}

/// A [NetworkStream](NetworkStream) that can be used server-side.
pub trait ServerStream: NetworkStream {}

/// A read-write stream between the client and the server.
#[async_trait]
pub trait NetworkStream: Send + Sync + 'static {
    /// A read half of this stream.
    type ReadHalf: ReadStream;
    /// A write half of this stream.
    type WriteHalf: WriteStream;

    /// Splits this stream into read and write half to use them in different futures.
    async fn into_split(self) -> io::Result<(Self::ReadHalf, Self::WriteHalf)>;

    /// Returns the socket address of the remote peer of this TCP connection.
    fn peer_addr(&self) -> SocketAddr;

    /// Returns the socket address of the local half of this TCP connection.
    fn local_addr(&self) -> SocketAddr;
}

#[async_trait]
pub trait ReadStream: Send + Sync + 'static {
    /// Fills the whole buffer with bytes in this stream.
    async fn read_exact(&mut self, buffer: &mut [u8]) -> io::Result<()>;

    /// Reads a single packet from this stream.
    ///
    /// You shouldn't override this method unless you know what you're doing.
    async fn receive<ReceivingPacket, SendingPacket, S, LS>(
        &mut self,
        serializer: &S,
        length_serializer: &LS,
    ) -> Result<ReceivingPacket, ReceiveError<ReceivingPacket, SendingPacket, S, LS>>
    where
        ReceivingPacket: Send + Sync + Debug + 'static,
        SendingPacket: Send + Sync + Debug + 'static,
        S: Serializer<ReceivingPacket, SendingPacket>,
        LS: PacketLengthSerializer,
    {
        let mut buf = Vec::new();
        let mut length = Err(PacketLengthDeserializationError::NeedMoreBytes(LS::SIZE));
        while let Err(PacketLengthDeserializationError::NeedMoreBytes(amt)) = length {
            let mut tmp = vec![0; amt];
            self.read_exact(&mut tmp).await.map_err(ReceiveError::Io)?;
            buf.extend(tmp);
            length = length_serializer.deserialize_packet_length(&buf);
        }

        match length {
            Ok(length) => {
                if length > MAX_PACKET_SIZE.load(Ordering::Relaxed) {
                    Err(ReceiveError::PacketTooBig)
                } else {
                    let mut buf = vec![0; length];
                    self.read_exact(&mut buf).await.map_err(ReceiveError::Io)?;
                    Ok(serializer
                        .deserialize(&buf)
                        .map_err(ReceiveError::Deserialization)?)
                }
            }
            Err(PacketLengthDeserializationError::Err(err)) => {
                Err(ReceiveError::LengthDeserialization(err))
            }
            Err(PacketLengthDeserializationError::NeedMoreBytes(_)) => unreachable!(),
        }
    }
}

pub enum ReceiveError<ReceivingPacket, SendingPacket, S, LS>
where
    ReceivingPacket: Send + Sync + Debug + 'static,
    SendingPacket: Send + Sync + Debug + 'static,
    S: Serializer<ReceivingPacket, SendingPacket>,
    LS: PacketLengthSerializer,
{
    Io(io::Error),
    Deserialization(S::Error),
    LengthDeserialization(LS::Error),
    PacketTooBig,
    NoConnection(io::Error),
}

impl<ReceivingPacket, SendingPacket, S, LS> Debug
    for ReceiveError<ReceivingPacket, SendingPacket, S, LS>
where
    ReceivingPacket: Send + Sync + Debug + 'static,
    SendingPacket: Send + Sync + Debug + 'static,
    S: Serializer<ReceivingPacket, SendingPacket>,
    LS: PacketLengthSerializer,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ReceiveError::Io(error) => write!(f, "ReceiveError::Io({error:?})"),
            ReceiveError::Deserialization(error) => {
                write!(f, "ReceiveError::Deserialization({error:?})")
            }
            ReceiveError::LengthDeserialization(error) => {
                write!(f, "ReceiveError::LengthDeserialization({error:?})")
            }
            ReceiveError::PacketTooBig => write!(f, "ReceiveError::PacketTooBig"),
            ReceiveError::NoConnection(error) => write!(f, "ReceiveError::NoConnection({error:?})"),
        }
    }
}

#[async_trait]
pub trait WriteStream: Send + Sync + 'static {
    /// Writes the whole buffer to the stream.
    async fn write_all(&mut self, buffer: &[u8]) -> io::Result<()>;

    /// Writes a packet to this stream.
    ///
    /// You shouldn't override this method unless you know what you're doing.
    async fn send<ReceivingPacket, SendingPacket, S, LS>(
        &mut self,
        packet: SendingPacket,
        serializer: &S,
        length_serializer: &LS,
    ) -> io::Result<()>
    where
        ReceivingPacket: Send + Sync + Debug + 'static,
        SendingPacket: Send + Sync + Debug + 'static,
        S: Serializer<ReceivingPacket, SendingPacket>,
        LS: PacketLengthSerializer,
    {
        let serialized = serializer
            .serialize(packet)
            .expect("Error serializing packet");
        let mut buf = length_serializer
            .serialize_packet_length(serialized.len())
            .expect("Error serializing packet length");
        buf.write_all(&serialized)?;
        self.write_all(&buf).await?;
        Ok(())
    }
}
