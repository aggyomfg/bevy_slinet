//! Encodes packet-length prefixes for stream transports; UDP does not use them.

use std::error::Error;
use std::marker::PhantomData;

/// Defines length-prefixed framing for stream transports.
pub trait PacketLengthSerializer: Send + Sync + 'static {
    /// Describes a length the codec cannot represent.
    type Error: Error + Send + Sync;

    /// Initial prefix width in bytes; decoders can request additional bytes.
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

/// Distinguishes an incomplete prefix from one the codec cannot decode.
#[derive(Clone, Debug, thiserror::Error)]
pub enum PacketLengthDeserializationError<E> {
    /// Counts additional bytes needed beyond the prefix already supplied.
    #[error("Packet length prefix needs {0} more bytes")]
    NeedMoreBytes(usize),
    /// Rejects the prefix instead of requesting more bytes.
    #[error(transparent)]
    Err(#[from] E),
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

/// Encodes the packet length as a little-endian number.
#[derive(Default)]
pub struct LittleEndian<N>(PhantomData<N>);

/// Encodes the packet length as a big-endian number.
#[derive(Default)]
pub struct BigEndian<N>(PhantomData<N>);

macro_rules! impl_pls {
    ($typ: ident $(<$generics: tt>)? = $to: ident & $from: ident => $number: ty) => {
        impl PacketLengthSerializer for $typ $(<$generics>)? {
            type Error = PacketTooLargeError;

            const SIZE: usize = <$number>::BITS as usize / 8;

            fn serialize_packet_length(&self, length: usize) -> Result<Vec<u8>, Self::Error> {
                let value = <$number>::try_from(length).map_err(|_| PacketTooLargeError {
                    length,
                    max_length: usize::try_from(<$number>::MAX).unwrap_or(usize::MAX),
                })?;
                Ok(value.$to().to_vec())
            }

            fn deserialize_packet_length(
                &self,
                buffer: &[u8],
            ) -> Result<usize, PacketLengthDeserializationError<Self::Error>> {
                let Some(prefix) = buffer.get(..Self::SIZE) else {
                    return Err(PacketLengthDeserializationError::NeedMoreBytes(Self::SIZE - buffer.len()));
                };
                let mut bytes = [0; size_of::<$number>()];
                bytes.copy_from_slice(prefix);
                usize::try_from(<$number>::$from(bytes)).map_err(|_| {
                    PacketLengthDeserializationError::Err(PacketTooLargeError {
                        // The actual wire value exceeds the range of this error's usize fields.
                        length: usize::MAX,
                        max_length: usize::MAX,
                    })
                })
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
    #[test]
    fn incomplete_prefix_requests_missing_bytes() {
        assert!(matches!(
            LittleEndian::<u32>::default().deserialize_packet_length(&[1, 2]),
            Err(PacketLengthDeserializationError::NeedMoreBytes(2))
        ));
    }

    #[test]
    fn prefix_exceeding_usize_is_rejected() {
        for codec in [false, true] {
            let result = if codec {
                BigEndian::<u128>::default().deserialize_packet_length(&u128::MAX.to_be_bytes())
            } else {
                LittleEndian::<u128>::default().deserialize_packet_length(&u128::MAX.to_le_bytes())
            };
            assert!(matches!(
                result,
                Err(PacketLengthDeserializationError::Err(_))
            ));
        }
    }
}
