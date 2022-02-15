#![deny(rustdoc::broken_intra_doc_links)]
#![deny(missing_docs)]
#![cfg_attr(not(doctest), doc = include_str!("../README.md"))]

use std::fmt::Debug;

use crate::packet_length_serializer::PacketLengthSerializer;
use crate::protocol::Protocol;
use crate::serializer::Serializer;

#[cfg(feature = "client")]
pub mod client;
pub mod connection;
pub mod packet_length_serializer;
pub mod protocol;
pub mod protocols;
pub mod serializer;
pub mod serializers;
#[cfg(feature = "server")]
pub mod server;

/// [`Labels`](bevy::ecs::schedule::SystemLabel) in [`bevy`] are used for system ordering.
/// See [System Order of Execution][cheatbook_order] on unofficial bevy cheatbook for details.
/// For more details on what each label means, refer to [`client`](crate::client) or [`server`](crate::server) source code
///
/// [cheatbook_order]: https://bevy-cheatbook.github.io/programming/system-order.html
#[derive(bevy::ecs::schedule::SystemLabel, Clone, Hash, Debug, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum SystemLabels {
    ClientPacketReceive,
    ClientConnectionEstablish,
    ClientConnectionRemove,
    ClientConnectionRequest,
    ServerConnectionAdd,
    ServerAcceptNewConnections,
    ServerAcceptNewPackets,
    ServerRemoveConnections,
    SetMaxPacketSize,
}

/// A server plugin config.
pub trait ServerConfig: Send + Sync + 'static {
    /// A client-side packet type.
    type ClientPacket: Send + Sync + Debug + 'static;
    /// A server-side packet type.
    type ServerPacket: Send + Sync + Debug + 'static;
    /// The connection's protocol.
    type Protocol: Protocol;
    /// A packet serializer.
    type Serializer: Serializer<Self::ClientPacket, Self::ServerPacket> + Default;
    /// A packet length serializer
    type LengthSerializer: PacketLengthSerializer + Default;
}

/// A client plugin config.
pub trait ClientConfig: Send + Sync + 'static {
    /// A client-side packet type.
    type ClientPacket: Send + Sync + Debug + 'static;
    /// A server-side packet type.
    type ServerPacket: Send + Sync + Debug + 'static;
    /// The connection's protocol.
    type Protocol: Protocol;
    /// A packet serializer.
    type Serializer: Serializer<Self::ServerPacket, Self::ClientPacket> + Default;
    /// A packet length serializer
    type LengthSerializer: PacketLengthSerializer + Default;
}
