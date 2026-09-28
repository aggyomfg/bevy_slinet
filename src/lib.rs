#![deny(rustdoc::broken_intra_doc_links)]
#![cfg_attr(not(debug_assertions), deny(missing_docs))]
#![cfg_attr(not(doctest), doc = include_str!("../README.md"))]

use std::{error::Error, fmt::Debug};

use crate::protocols::protocol::Protocol;
use crate::serializers::packet_length_serializer::PacketLengthSerializer;
use bevy::prelude::SystemSet;
use serializers::serializer::SerializerAdapter;

#[cfg(feature = "client")]
pub mod client;
pub mod connection;
#[cfg(any(feature = "client", feature = "server", feature = "protocol_udp", test))]
pub(crate) mod packet_queue;
pub mod protocols;
pub mod serializers;
#[cfg(feature = "server")]
pub mod server;

// Preserve the original public module paths.
pub use protocols::protocol;
pub use serializers::{packet_length_serializer, serializer};

#[cfg(all(
    test,
    feature = "client",
    feature = "server",
    any(feature = "protocol_tcp", feature = "protocol_udp"),
    feature = "serializer_bitcode_serde"
))]
mod tests;

/// Exposes networking phases so application systems can order their work around packet events.
///
/// Each plugin processes establishment and removal in one FIFO system during `PreUpdate`.
/// Its establishment and removal labels select that same system. Systems ordered around
/// removal must therefore also run in `PreUpdate`; ordering does not cross schedules.
#[derive(Clone, Debug, Eq, Hash, PartialEq, SystemSet)]
pub enum SystemSets {
    /// Publishes client packet events during `PostUpdate`.
    ClientPacketReceive,
    /// Processes client establishment and closure during `PreUpdate`.
    ClientConnectionEstablish,
    /// The same lifecycle phase as [`Self::ClientConnectionEstablish`].
    ClientConnectionRemove,
    /// Legacy label; built-in connection requests are handled by observers.
    ClientConnectionRequest,
    /// Legacy label; use [`Self::ServerAcceptNewConnections`] for server lifecycle events.
    ServerConnectionAdd,
    /// Processes server establishment and closure during `PreUpdate`.
    ServerAcceptNewConnections,
    /// Publishes server packet events after lifecycle processing during `PreUpdate`.
    ServerAcceptNewPackets,
    /// The same lifecycle phase as [`Self::ServerAcceptNewConnections`].
    ServerRemoveConnections,
    /// Synchronizes app-local receive limits during `Startup` and `Update`.
    SetMaxPacketSize,
    /// Reports a missing receive-size limit during `Startup`.
    MaxPacketSizeWarning,
}

/// A server plugin config.
pub trait ServerConfig: Send + Sync + 'static {
    /// A client-side packet type.
    type ClientPacket: Send + Sync + Debug + 'static;
    /// A server-side packet type.
    type ServerPacket: Send + Sync + Debug + 'static;
    /// The connection's protocol.
    type Protocol: Protocol;
    /// Error type for encoding operations
    type EncodeError: Error + Send + Sync;
    /// Error type for decoding operations
    type DecodeError: Error + Send + Sync;
    /// A packet serializer.
    fn build_serializer() -> SerializerAdapter<
        Self::ClientPacket,
        Self::ServerPacket,
        Self::EncodeError,
        Self::DecodeError,
    >;
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
    /// Error type for encoding operations
    type EncodeError: Error + Send + Sync;
    /// Error type for decoding operations
    type DecodeError: Error + Send + Sync;
    /// A packet serializer.
    fn build_serializer() -> SerializerAdapter<
        Self::ServerPacket,
        Self::ClientPacket,
        Self::EncodeError,
        Self::DecodeError,
    >;
    /// A packet length serializer
    type LengthSerializer: PacketLengthSerializer + Default;
}
