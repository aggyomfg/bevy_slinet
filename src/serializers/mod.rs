//! Implemented serializers:
#![cfg_attr(
    feature = "serializer_bincode",
    doc = "- [`bincode`] - Native bincode 2.0 (fastest, use `serializer_bincode` feature)"
)]
#![cfg_attr(
    not(feature = "serializer_bincode"),
    doc = "- `bincode` - Native bincode 2.0 (fastest, use `serializer_bincode` feature)"
)]
#![cfg_attr(
    feature = "serializer_bincode_serde",
    doc = "- [`bincode_serde`] - Serde-compatible bincode 2.0 (use `serializer_bincode_serde` feature)"
)]
#![cfg_attr(
    not(feature = "serializer_bincode_serde"),
    doc = "- `bincode_serde` - Serde-compatible bincode 2.0 (use `serializer_bincode_serde` feature)"
)]
#![cfg_attr(
    feature = "serializer_bincode",
    doc = "- [`custom_crypt`] - Example encryption serializer (use `serializer_bincode` feature)"
)]
#![cfg_attr(
    not(feature = "serializer_bincode"),
    doc = "- `custom_crypt` - Example encryption serializer (use `serializer_bincode` feature)"
)]

#[cfg(feature = "serializer_bincode")]
pub mod bincode;
#[cfg(feature = "serializer_bincode_serde")]
pub mod bincode_serde;
#[cfg(feature = "serializer_bincode")]
pub mod custom_crypt;
