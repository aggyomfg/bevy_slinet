//! Protocol traits and implemented protocols:
#![cfg_attr(feature = "protocol_tcp", doc = "- [`tcp`] (`protocol_tcp` feature)")]
#![cfg_attr(
    not(feature = "protocol_tcp"),
    doc = "- `tcp` (`protocol_tcp` feature)"
)]
#![cfg_attr(feature = "protocol_udp", doc = "- [`udp`] (`protocol_udp` feature)")]
#![cfg_attr(
    not(feature = "protocol_udp"),
    doc = "- `udp` (`protocol_udp` feature)"
)]

pub mod protocol;

mod framing;
mod packet;
mod transport;

#[cfg(feature = "protocol_tcp")]
pub mod tcp;

#[cfg(feature = "protocol_udp")]
pub mod udp;
