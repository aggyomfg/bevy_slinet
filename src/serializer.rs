//! Implement the [`ReadOnlySerializer`] or [`MutableSerializer`] trait for serialzer and build it in config refer
//! BincodeSerializer to check how to do this.

use core::fmt::Debug;
use std::{
    error::Error,
    fmt::{self, Display},
    sync::{Arc, Mutex},
};

// Serializer trait defines the core functionality for serializing and deserializing packets,
// ensuring compatibility with multi-threaded contexts as it requires the Send and Sync bounds.
pub trait Serializer<ReceivingPacket, SendingPacket>: Send + Sync + 'static
where
    ReceivingPacket: Send + Sync + Debug + 'static,
    SendingPacket: Send + Sync + Debug + 'static,
{
    type Error: Error + Send + Sync; // Defines a custom error type that must also be thread-safe.

    // Serializes a packet into bytes to be sent over a network. The method takes ownership of the packet
    // and a peer address, returning either a byte vector or an error if serialization fails.
    fn serialize(&self, packet: SendingPacket) -> Result<Vec<u8>, Self::Error>;
    // Deserializes bytes received from a network back into a packet structure.
    fn deserialize(&self, data: &[u8]) -> Result<ReceivingPacket, Self::Error>;
}

// DefaultSerializationError is a minimal implementation of an error that might occur during the
// serialization process. Using Display trait for user-friendly error messaging.
#[derive(Debug)]
pub struct DefaultSerializationError;
impl Display for DefaultSerializationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SerializationFailed")
    }
}

impl Error for DefaultSerializationError {}

// SerializerAdapter allows for flexibility in serializer implementation; supporting both immutable
// and mutable serialization strategies.
pub enum SerializerAdapter<ReceivingPacket, SendingPacket, E = DefaultSerializationError>
where
    E: Error + Send + Sync,
{
    ReadOnly(Arc<dyn ReadOnlySerializer<ReceivingPacket, SendingPacket, Error = E>>),
    Mutable(Arc<Mutex<dyn MutableSerializer<ReceivingPacket, SendingPacket, Error = E>>>),
}

// Implementing Serializer for SerializerAdapter provides a concrete example of polymorphism,
// enabling different serialization strategies under the same interface.
impl<ReceivingPacket, SendingPacket, E> Serializer<ReceivingPacket, SendingPacket>
    for SerializerAdapter<ReceivingPacket, SendingPacket, E>
where
    SendingPacket: Send + Sync + Debug + 'static,
    ReceivingPacket: Send + Sync + Debug + 'static,
    E: Error + Send + Sync + 'static,
{
    type Error = E;

    // Depending on the adapter's type, serialization can either directly pass the data or
    // require locking a mutex to ensure thread-safety in mutable contexts.
    fn serialize(&self, packet: SendingPacket) -> Result<Vec<u8>, Self::Error> {
        match self {
            SerializerAdapter::ReadOnly(serializer) => serializer.serialize(packet),
            SerializerAdapter::Mutable(serializer) => serializer.lock().unwrap().serialize(packet),
        }
    }

    // Deserialization behaves similarly to serialization, respecting the adapter's type.
    fn deserialize(&self, data: &[u8]) -> Result<ReceivingPacket, Self::Error> {
        match self {
            SerializerAdapter::ReadOnly(serializer) => serializer.deserialize(data),
            SerializerAdapter::Mutable(serializer) => serializer.lock().unwrap().deserialize(data),
        }
    }
}

/// ReadOnlySerializer is designed for scenarios where the data structure does not change,
/// ensuring efficient and thread-safe operations without the overhead of locking mechanisms.
pub trait ReadOnlySerializer<ReceivingPacket, SendingPacket>: Send + Sync + 'static {
    type Error: Error + Send + Sync;
    fn serialize(&self, packet: SendingPacket) -> Result<Vec<u8>, Self::Error>;
    fn deserialize(&self, buffer: &[u8]) -> Result<ReceivingPacket, Self::Error>;
}
/// MutableSerializer supports scenarios where serialization state needs to be altered during operation,
/// useful in cases like cryptographic transformations where state is critical.
pub trait MutableSerializer<ReceivingPacket, SendingPacket>: Send + Sync + 'static {
    type Error: Error + Send + Sync;
    fn serialize(&mut self, p: SendingPacket) -> Result<Vec<u8>, Self::Error>;
    fn deserialize(&mut self, buf: &[u8]) -> Result<ReceivingPacket, Self::Error>;
}
