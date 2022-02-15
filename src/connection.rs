//! This module contains structs that are used connection handling.

use bevy::prelude::Res;
use std::fmt::Debug;
use std::io;
use std::io::Write;
use std::marker::PhantomData;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender};

use crate::packet_length_serializer::{PacketLengthDeserializationError, PacketLengthSerializer};
use crate::protocol::NetworkStream;
use crate::serializer::Serializer;

/// The ecs-side connection struct. There is 2 structs,
/// one raw (with the stream, runs on another thread),
/// and ecs that can be cheaply cloned and interacts with
/// the raw connection via [`crossbeam_channel`].
pub struct EcsConnection<SendingPacket>
where
    SendingPacket: Send + Sync + Debug + 'static,
{
    pub(crate) id: ConnectionId,
    pub(crate) packet_tx: Sender<SendingPacket>,
    pub(crate) local_addr: SocketAddr,
    pub(crate) peer_addr: SocketAddr,
}

impl<SendingPacket> Clone for EcsConnection<SendingPacket>
where
    SendingPacket: Send + Sync + Debug + 'static,
{
    fn clone(&self) -> Self {
        EcsConnection {
            id: self.id,
            packet_tx: self.packet_tx.clone(),
            local_addr: self.local_addr,
            peer_addr: self.peer_addr,
        }
    }
}

impl<SendingPacket> EcsConnection<SendingPacket>
where
    SendingPacket: Send + Sync + Debug + 'static,
{
    /// Returns this connection's [`ID`](ConnectionId).
    pub fn id(&self) -> ConnectionId {
        self.id
    }

    /// Returns the socket address of the remote peer of this TCP connection.
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    /// Returns the socket address of the local half of this TCP connection.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Sends a packet to the server.
    pub fn send(&self, packet: SendingPacket) {
        self.packet_tx.send(packet).unwrap();
    }
}

pub(crate) struct RawConnection<ReceivingPacket, SendingPacket, NS, S, LS>
where
    ReceivingPacket: Send + Sync + Debug + 'static,
    SendingPacket: Send + Sync + Debug + 'static,
    NS: NetworkStream,
    S: Serializer<ReceivingPacket, SendingPacket>,
    LS: PacketLengthSerializer,
{
    pub stream: NS,
    pub serializer: Arc<S>,
    pub packet_length_serializer: Arc<LS>,
    pub packets_rx: Receiver<SendingPacket>,
    pub id: ConnectionId,
    pub _receive_packet: PhantomData<ReceivingPacket>,
    pub _send_packet: PhantomData<SendingPacket>,
}

/// A connection ID is an unique connection identifier that is mainly used
/// in servers with multiple clients. This ID should only be used locally
/// and is not meant to be exposed to the other side or stored in a database.
/// Client-side ConnectionId and server-side ConnectionId are NOT the same.
/// ConnectionId is basically an static AtomicUsize counter, so it resets
/// every server restart. If there are multiple clients/servers running
/// (like in multiple_connections example), they'll have a single connection
/// counter that increments for every clientside/serverside connection.
#[derive(
    Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Debug, Hash, bevy::ecs::component::Component,
)]
pub struct ConnectionId(usize);

impl ConnectionId {
    /// Creates and returns a new, unique [`ConnectionId`].
    /// See the source code for implementation details.
    pub fn next() -> ConnectionId {
        static CONNECTION_ID: AtomicUsize = AtomicUsize::new(0);

        ConnectionId(CONNECTION_ID.fetch_add(1, Ordering::Relaxed))
    }
}

pub(crate) static MAX_PACKET_SIZE: AtomicUsize = AtomicUsize::new(usize::MAX);

/// We can't set it as a field in [`ClientConfig`](crate::ClientConfig) or [`ServerConfig`](crate::ServerConfig)
/// because using trait consts as const generics require `generic_const_exprs` feature. You should set
/// this resource to avoid out-of-memory attacks (where a client sends a packet with length-prefix of
/// 100000000000 bytes and bevy_slinet tries to allocate a buffer of that size).
#[derive(Copy, Clone)]
pub struct MaxPacketSize(pub usize);

impl<ReceivingPacket, SendingPacket, NS, S, LS>
    RawConnection<ReceivingPacket, SendingPacket, NS, S, LS>
where
    ReceivingPacket: Send + Sync + Debug + 'static,
    SendingPacket: Send + Sync + Debug + 'static,
    NS: NetworkStream,
    S: Serializer<ReceivingPacket, SendingPacket>,
    LS: PacketLengthSerializer,
{
    pub fn new(
        stream: NS,
        serializer: S,
        packet_length_serializer: LS,
        packets_rx: Receiver<SendingPacket>,
    ) -> Self {
        stream.set_nonblocking();
        Self {
            stream,
            serializer: Arc::new(serializer),
            packet_length_serializer: Arc::new(packet_length_serializer),
            packets_rx,
            id: ConnectionId::next(),
            _receive_packet: PhantomData,
            _send_packet: PhantomData,
        }
    }

    pub fn send(&mut self, packet: SendingPacket) -> io::Result<()> {
        let buf = self.serialize_packet(packet)?;
        self.stream.write_all(&buf)?;
        Ok(())
    }

    pub fn serialize_packet(&self, packet: SendingPacket) -> io::Result<Vec<u8>> {
        let serialized = self
            .serializer
            .serialize(packet)
            .expect("Error serializing packet");
        let mut buf = self
            .packet_length_serializer
            .serialize_packet_length(serialized.len())
            .expect("Error serializing packet length");
        buf.write_all(&serialized)?;
        Ok(buf)
    }

    pub fn receive(
        &mut self,
    ) -> Result<ReceivingPacket, ReceiveError<ReceivingPacket, SendingPacket, S, LS>> {
        let mut size = 0;
        let mut length = Err(PacketLengthDeserializationError::NeedMoreBytes(LS::SIZE));
        while let Err(PacketLengthDeserializationError::NeedMoreBytes(amt)) = length {
            size += amt;
            let mut buf = vec![0; size];
            self.stream
                .try_peek_exact(&mut buf)
                .map_err(ReceiveError::Io)?;
            length = self
                .packet_length_serializer
                .deserialize_packet_length(&buf);
        }

        match length {
            Ok(length) => {
                if length > MAX_PACKET_SIZE.load(Ordering::Relaxed) {
                    Err(ReceiveError::PacketTooBig)
                } else {
                    let mut buf = vec![0; length + size];
                    self.stream.read_exact(&mut buf).map_err(ReceiveError::Io)?;
                    Ok(self
                        .serializer
                        .deserialize(&buf[size..])
                        .map_err(ReceiveError::Deserialization)?)
                }
            }
            Err(PacketLengthDeserializationError::Err(err)) => {
                Err(ReceiveError::LengthDeserialization(err))
            }
            Err(PacketLengthDeserializationError::NeedMoreBytes(_)) => unreachable!(),
        }
    }

    pub fn id(&self) -> ConnectionId {
        self.id
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.stream.local_addr()
    }

    pub fn peer_addr(&self) -> SocketAddr {
        self.stream.peer_addr()
    }
}

pub(crate) enum ReceiveError<ReceivingPacket, SendingPacket, S, LS>
where
    ReceivingPacket: Send + Sync + Debug + 'static,
    SendingPacket: Send + Sync + Debug + 'static,
    S: Serializer<ReceivingPacket, SendingPacket>,
    LS: PacketLengthSerializer,
{
    Io(std::io::Error),
    Deserialization(S::Error),
    LengthDeserialization(LS::Error),
    PacketTooBig,
}

pub(crate) fn max_packet_size_system(max_packet_size: Option<Res<MaxPacketSize>>) {
    match max_packet_size {
        Some(res) if res.is_changed() => {
            MAX_PACKET_SIZE.store(res.0, Ordering::Relaxed);
        }
        _ => (),
    }
}
