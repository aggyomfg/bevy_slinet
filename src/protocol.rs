//! Implement [`Protocol`] to create your own protocol implementation and use
//! it in [`ServerConfig`](crate::ServerConfig) or [`ClientConfig`](crate::ClientConfig).
//!
//! Built-in protocols are listed in the [`protocols`](crate::protocols) module.

use std::net::{SocketAddr, ToSocketAddrs};

/// In order to simplify protocol switching and implementation, there is a [`Protocol`] trait.
/// Implement it or use built-in [`protocols`](crate::protocols).
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
    fn bind<A>(addr: A) -> std::io::Result<Self::Listener>
    where
        A: ToSocketAddrs;

    /// Creates and runs a non-blocking [Listener](Self::Listener).
    fn create_listener<A>(addr: A) -> std::io::Result<Self::Listener>
    where
        A: ToSocketAddrs,
    {
        let listener = Self::bind(addr)?;
        log::debug!("Created and started a listener at {:?}", listener.address());
        listener.set_nonblocking();
        Ok(listener)
    }

    /// Connect to the server at specified address.
    fn connect_to_server<A>(addr: A) -> std::io::Result<Self::ClientStream>
    where
        A: ToSocketAddrs,
    {
        let stream = Self::ClientStream::connect(addr)?;
        log::debug!("Connected to a server at {:?}", stream.peer_addr());
        stream.set_nonblocking();
        Ok(stream)
    }
}

/// A listener that accepts connections from clients.
pub trait Listener<S>: Send + Sync + 'static
where
    S: ServerStream,
{
    /// Returns a [ServerStream](ServerStream) if a client wants to connect.
    fn accept(&self) -> std::io::Result<(S, SocketAddr)>;
    /// The listener should never block if this method was called.
    fn set_nonblocking(&self);
    /// Get the local address that this listeners listens at.
    fn address(&self) -> SocketAddr;
}

/// A [NetworkStream](NetworkStream) that can be used client-side.
pub trait ClientStream: NetworkStream {
    /// Connects to a server.
    fn connect<A>(addr: A) -> std::io::Result<Self>
    where
        Self: Sized,
        A: ToSocketAddrs;
}

/// A [NetworkStream](NetworkStream) that can be used server-side.
pub trait ServerStream: NetworkStream {}

/// A read-write stream between the client and the server.
pub trait NetworkStream: Send + Sync + 'static {
    /// The stream should never block if this method was called.
    fn set_nonblocking(&self);

    /// Fill the whole buffer with bytes in this stream, not consuming them.
    /// This means the next [read_exact](Self::read_exact) call should start
    /// with the same peeked bytes.
    fn try_peek_exact(&self, buffer: &mut [u8]) -> std::io::Result<()>;

    /// Fills the whole buffer with bytes in this stream. Should return an
    /// error if there's not enough bytes available.
    fn read_exact(&mut self, buffer: &mut [u8]) -> std::io::Result<()>;

    /// Writes the whole buffer to the stream.
    fn write_all(&mut self, buffer: &[u8]) -> std::io::Result<()>;

    /// Returns the socket address of the remote peer of this TCP connection.
    fn peer_addr(&self) -> SocketAddr;

    /// Returns the socket address of the local half of this TCP connection.
    fn local_addr(&self) -> SocketAddr;
}
