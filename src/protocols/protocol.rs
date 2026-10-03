//! Implement [`Protocol`] to create your own protocol implementation and use
//! it in [`ServerConfig`](crate::ServerConfig) or [`ClientConfig`](crate::ClientConfig).
//!
//! Built-in protocols are listed in the [`protocols`](crate::protocols) module.
//!
//! [`NetworkStream`] splits into a [`PacketReader`] and a [`PacketWriter`].
//! Byte transports implement [`ReadStream`] and [`WriteStream`] and wrap their
//! halves in [`FramedReader`] and [`FramedWriter`] to supply length-prefix framing:
//!
//! ```
//! use bevy_slinet::protocol::{
//!     FramedReader, FramedWriter, PacketReader, PacketWriter, ReadStream, WriteStream,
//! };
//!
//! fn frame<R: ReadStream, W: WriteStream>(
//!     read: R,
//!     write: W,
//! ) -> (impl PacketReader, impl PacketWriter) {
//!     (FramedReader::new(read), FramedWriter::new(write))
//! }
//! ```
//!
//! Transports with native packet boundaries implement the packet traits directly.

use bevy::log;
use std::io;
use std::net::SocketAddr;

use async_trait::async_trait;

pub use super::framing::{FramedReader, FramedWriter, ReadStream, WriteStream};
pub use super::packet::{PacketReader, PacketWriter, ReceiveError};
pub use super::transport::{QueueDropReason, TransportHandle};

/// In order to simplify protocol switching and implementation, there is a [`Protocol`] trait.
/// Implement it or use built-in [`protocols`](crate::protocols).
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
pub trait Protocol: Send + Sync + 'static {
    /// Shared controls and diagnostics for this protocol's connections.
    type Handle: TransportHandle;
    /// A server-side listener type.
    type Listener: Listener<Stream = Self::ServerStream>;
    /// A server-side network stream. It can be different from [`Self::ClientStream`]
    type ServerStream: ServerStream<Handle = Self::Handle>;
    /// A client-side network stream. It can be different from [`Self::ServerStream`]
    type ClientStream: ClientStream<Handle = Self::Handle>;

    /// `true` if packets may be lost, duplicated or reordered, as with UDP.
    const DATAGRAM: bool = false;

    /// Creates a [Listener](Self::Listener).
    async fn bind(addr: SocketAddr) -> io::Result<Self::Listener>;

    /// Connect to the server at specified address.
    async fn connect_to_server(addr: SocketAddr) -> io::Result<Self::ClientStream> {
        let stream = Self::ClientStream::connect(addr).await?;
        log::debug!("Connected to a server at {:?}", stream.peer_addr());
        Ok(stream)
    }
}

/// A listener that accepts connections from clients.
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
pub trait Listener {
    /// A [`ServerStream`] that is returned by [`Self::accept()`]
    type Stream: ServerStream;

    /// Returns a [ServerStream](ServerStream) when a client wants to connect.
    async fn accept(&self) -> io::Result<Self::Stream>;

    /// Returns the bound endpoint, including the assigned port when bound to port zero.
    fn address(&self) -> SocketAddr;

    /// Releases listener-side state after a connection’s receive task closes.
    fn handle_disconnection(&self, _peer_addr: SocketAddr) {}
}

/// A [NetworkStream](NetworkStream) that can be used client-side.
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
pub trait ClientStream: NetworkStream {
    /// Connects to a server.
    async fn connect(addr: SocketAddr) -> io::Result<Self>
    where
        Self: Sized;
}

/// A [`NetworkStream`] that can be used server-side.
pub trait ServerStream: NetworkStream {}

/// A connection that splits into packet receive and send halves.
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
pub trait NetworkStream: Send + Sync + 'static {
    /// Shared controls and diagnostics for this connection.
    type Handle: TransportHandle;
    /// The packet-receiving half of this connection.
    type ReadHalf: PacketReader;
    /// The packet-sending half of this connection.
    type WriteHalf: PacketWriter;

    /// Splits this stream into read and write half to use them in different futures.
    async fn into_split(self) -> io::Result<(Self::ReadHalf, Self::WriteHalf)>;

    /// Returns the socket address of the remote peer.
    fn peer_addr(&self) -> SocketAddr;

    /// Returns the socket address of the local endpoint.
    fn local_addr(&self) -> SocketAddr;

    /// Returns a clone of this connection's shared controls and diagnostics.
    fn transport(&self) -> Self::Handle;
}

#[cfg(test)]
#[path = "protocol_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "framing_tests.rs"]
mod send_tests;
