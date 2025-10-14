//! Implemented serializers:
//! - [`bincode`] - Native bincode 2.0 (fastest, use `serializer_bincode` feature)
//! - [`bincode_serde`] - Serde-compatible bincode 2.0 (use `serializer_bincode_serde` feature)
//! - [`custom_crypt`] - Example encryption serializer

#[cfg(feature = "serializer_bincode")]
pub mod bincode;
#[cfg(feature = "serializer_bincode_serde")]
pub mod bincode_serde;
pub mod custom_crypt;
