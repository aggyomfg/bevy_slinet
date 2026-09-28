//! Encodes packet-length prefixes for stream transports; UDP does not use them.

use std::error::Error;
use std::marker::PhantomData;

/// Defines length-prefixed framing for stream transports.
pub trait PacketLengthSerializer: Send + Sync + 'static {
    /// The serializer's error type.
    type Error: Error + Send + Sync;

    /// The length's length in bytes. For u16 it would be 2.
    const SIZE: usize;

    /// Encodes the payload length in bytes.
    ///
    /// # Errors
    /// Returns an error when the length cannot be represented.
    fn serialize_packet_length(&self, length: usize) -> Result<Vec<u8>, Self::Error>;

    /// Parses a payload length from its prefix.
    ///
    /// # Errors
    /// Returns [`PacketLengthDeserializationError::NeedMoreBytes`] for an incomplete prefix,
    /// or [`PacketLengthDeserializationError::Err`] for invalid input.
    fn deserialize_packet_length(
        &self,
        buffer: &[u8],
    ) -> Result<usize, PacketLengthDeserializationError<Self::Error>>;
}

/// An error that [`PacketLengthSerializer::deserialize_packet_length`] may return.
#[derive(Clone, Debug)]
pub enum PacketLengthDeserializationError<E: Error> {
    /// The deserializer needs more bytes. This is useful for serializers
    /// with dynamic packet length length, e.g., 1 byte to store packet
    /// length for small packets, 2 bytes for larger packets)
    NeedMoreBytes(usize),
    /// Error
    Err(E),
}

/// Reports a payload length exceeding the selected integer representation.
#[derive(Debug, thiserror::Error)]
#[error("The packet is too large (length: {length}, max_length: {max_length})")]
pub struct PacketTooLargeError {
    /// Maximum payload length representable by the prefix, in bytes.
    pub max_length: usize,
    /// Rejected payload length in bytes.
    pub length: usize,
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_length_reports_actual_and_limit() {
        let error = LittleEndian::<u8>::default()
            .serialize_packet_length(256)
            .unwrap_err();
        assert_eq!(error.length, 256);
        assert_eq!(error.max_length, 255);
        assert_eq!(
            error.to_string(),
            "The packet is too large (length: 256, max_length: 255)"
        );
    }
}
