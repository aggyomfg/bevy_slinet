#![deny(rustdoc::broken_intra_doc_links)]
#![cfg_attr(not(debug_assertions), deny(missing_docs))]
#![cfg_attr(not(doctest), doc = include_str!("../README.md"))]

use std::{error::Error, fmt::Debug};

use crate::protocols::protocol::Protocol;
use crate::serializers::packet_length_serializer::PacketLengthSerializer;
use serializers::serializer::SerializerAdapter;

#[cfg(feature = "client")]
pub mod client;
pub mod connection;
#[cfg(any(feature = "client", feature = "server", feature = "protocol_udp", test))]
pub(crate) mod packet_queue;
pub mod protocols;
mod scheduling;
pub mod serializers;
pub use scheduling::{NetworkSystems, SystemSets};
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

/// Unstable fixtures for this repository's benchmarks, excluded from normal builds.
#[cfg(feature = "bench-internals")]
#[doc(hidden)]
#[path = "../benches/utils/mod.rs"]
pub mod bench_utils;
