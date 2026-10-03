//! Raw UDP dispatch/receive fixtures; socket creation is outside measurements.
#![allow(missing_docs, clippy::unwrap_used)]
#[allow(clippy::wildcard_imports)]
use super::*;
use crate::{
    connection::ReceiveLimits,
    packet_length_serializer::LittleEndian,
    protocol::{NetworkStream, PacketReader},
    serializer::Serializer,
};
use bevy::platform::time::Instant;
use std::{convert::Infallible, net::SocketAddr, sync::Arc};

struct LengthCodec;
impl Serializer<usize, usize> for LengthCodec {
    type EncodeError = Infallible;
    type DecodeError = Infallible;
    fn serialize(&self, _: usize) -> Result<Vec<u8>, Infallible> {
        Ok(Vec::new())
    }
    fn deserialize(&self, bytes: &[u8]) -> Result<usize, Infallible> {
        Ok(bytes.len())
    }
}
pub struct RawPeer {
    runtime: tokio::runtime::Runtime,
    listener: UdpNetworkListener,
    read: UdpReadHalf,
    _write: UdpWriteHalf,
    address: SocketAddr,
    codec: Arc<LengthCodec>,
    limits: ReceiveLimits,
}
impl Default for RawPeer {
    fn default() -> Self {
        Self::new()
    }
}
impl RawPeer {
    #[must_use]
    pub fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let listener = runtime
            .block_on(UdpNetworkListener::bind(
                ([127, 0, 0, 1], 0).into(),
                UdpOptions::DEFAULT,
            ))
            .unwrap();
        let address = ([127, 0, 0, 1], 1234).into();
        let stream = listener.dispatch(&[], address, Instant::now()).unwrap();
        let (read, write) = runtime.block_on(stream.into_split()).unwrap();
        let mut result = Self {
            runtime,
            listener,
            read,
            _write: write,
            address,
            codec: Arc::new(LengthCodec),
            limits: ReceiveLimits::default(),
        };
        assert_eq!(result.receive(), 0);
        result
    }
    fn receive(&mut self) -> usize {
        self.runtime
            .block_on(self.read.receive(
                Arc::clone(&self.codec),
                &LittleEndian::<u32>::default(),
                &self.limits,
            ))
            .unwrap()
    }
    pub fn roundtrip(&mut self, bytes: &[u8]) -> usize {
        assert!(self
            .listener
            .dispatch(bytes, self.address, Instant::now())
            .is_none());
        self.receive()
    }
    pub fn create_and_drop_peer(&self, bytes: &[u8]) {
        let stream = self
            .listener
            .dispatch(bytes, ([127, 0, 0, 1], 1235).into(), Instant::now())
            .unwrap();
        drop(stream);
    }
}
