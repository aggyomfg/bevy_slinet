//! In bevy_slinet all packets are prefixed by their length.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::marker::PhantomData;

/// This serializer controls how to serialize and deserialize the packet length
pub trait PacketLengthSerializer: Send + Sync + 'static {
    /// The serializer's error type.
    type Error: Error + Send + Sync;

    /// The length's length in bytes. For u16 it would be 2.
    const SIZE: usize;

    /// Serialize the packet's length
    fn serialize_packet_length(&self, length: usize) -> Result<Vec<u8>, Self::Error>;

    /// Deserialize the packet's length
    fn deserialize_packet_length(
        &self,
        buffer: &[u8],
    ) -> Result<usize, PacketLengthDeserializationError<Self::Error>>;
}

/// An error that [`PacketLengthSerializer::deserialize_packet_length`] may return.
#[derive(Debug, Clone)]
pub enum PacketLengthDeserializationError<E: Error> {
    /// The deserializer needs more bytes. This is useful for serializers
    /// with dynamic packet length length, e.g., 1 byte to store packet
    /// length for small packets, 2 bytes for larger packets)
    NeedMoreBytes(usize),
    /// Error
    Err(E),
}

/// Used by [`BigEndian`] and [`LittleEndian`], indicates that the outgoing packet's length
/// is greater than maximum allowed for this type of number
#[derive(Debug)]
pub struct PacketTooLargeError {
    /// The maximum allowed packet length. Usually a power of 2.
    pub max_length: usize,
    /// The actual packet's length.
    pub length: usize,
}

impl Display for PacketTooLargeError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "The packet is too large (length: {}, max_length: {})",
            self.max_length, self.length
        )
    }
}

impl Error for PacketTooLargeError {}

/// Serialize the packet length as a little-endian number.
#[derive(Default)]
pub struct LittleEndian<N>(PhantomData<N>);

/// Serialize the packet length as a big-endian number.
#[derive(Default)]
pub struct BigEndian<N>(PhantomData<N>);

macro_rules! impl_pls {
    ($typ: ident $(<$generics: tt>)? = $to: ident & $from: ident => $number: ty) => {
        impl PacketLengthSerializer for $typ $(<$generics>)? {
            type Error = PacketTooLargeError;

            const SIZE: usize = <$number>::BITS as usize / 8;

            fn serialize_packet_length(&self, length: usize) -> Result<Vec<u8>, Self::Error> {
                if length > <$number>::MAX as usize {
                    Err(PacketTooLargeError {
                        length,
                        max_length: <$number>::MAX as usize,
                    })
                } else {
                    Ok((length as $number).$to().to_vec())
                }
            }

            fn deserialize_packet_length(
                &self,
                buffer: &[u8],
            ) -> Result<usize, PacketLengthDeserializationError<Self::Error>> {
                Ok(<$number>::$from(buffer.try_into().unwrap()) as usize)
            }
        }
    };
}

macro_rules! impl_plss {
    ($endianness: ident = $to: ident & $from: ident: $($number: ty),+) => {
        $(
            impl_pls!($endianness<$number> = $to & $from => $number);
        )*
    };
}

impl_plss!(
    LittleEndian = to_le_bytes & from_le_bytes: u8,
    u16,
    u32,
    u64,
    u128
);
impl_plss!(
    BigEndian = to_be_bytes & from_be_bytes: u8,
    u16,
    u32,
    u64,
    u128
);
