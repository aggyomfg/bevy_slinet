//! Implemented serializers:
#![cfg_attr(
    feature = "serializer_bitcode",
    doc = "- [`bitcode`] - Native bitcode (fastest and most compact, use `serializer_bitcode` feature)"
)]
#![cfg_attr(
    not(feature = "serializer_bitcode"),
    doc = "- `bitcode` - Native bitcode (fastest and most compact, use `serializer_bitcode` feature)"
)]
#![cfg_attr(
    feature = "serializer_bitcode_serde",
    doc = "- [`bitcode_serde`] - Serde-compatible bitcode (use `serializer_bitcode_serde` feature)"
)]
#![cfg_attr(
    not(feature = "serializer_bitcode_serde"),
    doc = "- `bitcode_serde` - Serde-compatible bitcode (use `serializer_bitcode_serde` feature)"
)]
#![cfg_attr(
    feature = "serializer_bitcode",
    doc = "- [`custom_crypt`] - Example encryption serializer (use `serializer_bitcode` feature)"
)]
#![cfg_attr(
    not(feature = "serializer_bitcode"),
    doc = "- `custom_crypt` - Example encryption serializer (use `serializer_bitcode` feature)"
)]
//!
//! Kept for compatibility only (bincode is unmaintained upstream, prefer bitcode):
#![cfg_attr(
    feature = "serializer_bincode",
    doc = "- [`bincode`] - Native bincode 2.0 (use `serializer_bincode` feature)"
)]
#![cfg_attr(
    not(feature = "serializer_bincode"),
    doc = "- `bincode` - Native bincode 2.0 (use `serializer_bincode` feature)"
)]
#![cfg_attr(
    feature = "serializer_bincode_serde",
    doc = "- [`bincode_serde`] - Serde-compatible bincode 2.0 (use `serializer_bincode_serde` feature)"
)]
#![cfg_attr(
    not(feature = "serializer_bincode_serde"),
    doc = "- `bincode_serde` - Serde-compatible bincode 2.0 (use `serializer_bincode_serde` feature)"
)]

#[cfg(feature = "serializer_bincode")]
pub mod bincode;
#[cfg(feature = "serializer_bincode_serde")]
pub mod bincode_serde;
#[cfg(feature = "serializer_bitcode")]
pub mod bitcode;
#[cfg(feature = "serializer_bitcode_serde")]
pub mod bitcode_serde;
#[cfg(feature = "serializer_bitcode")]
pub mod custom_crypt;
