//! Internal benchmark fixtures. No compatibility guarantees.
#![allow(missing_docs, clippy::missing_errors_doc, clippy::must_use_candidate)]
pub use crate::client::bench_support::Fixture as ClientFixture;
use crate::connection::OverflowPolicy;
use tokio::sync::mpsc::error::{TryRecvError, TrySendError};

// Wrappers keep the production queue private; only this unstable bench API is exposed.
pub struct Sender<T>(crate::packet_queue::LossySender<T>);
pub struct Receiver<T>(crate::packet_queue::LossyReceiver<T>);
impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<T> Sender<T> {
    pub fn try_send(&self, value: T, bytes: usize) -> Result<Vec<T>, TrySendError<T>> {
        self.0.try_send(value, bytes)
    }
    pub async fn send(&self, value: T, bytes: usize) -> Result<(), TrySendError<T>> {
        self.0.send(value, bytes).await
    }
}
impl<T> Receiver<T> {
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        self.0.try_recv()
    }
    pub async fn recv(&mut self) -> Option<T> {
        self.0.recv().await
    }
}
pub fn lossy_channel<T>(
    items: usize,
    bytes: usize,
    policy: OverflowPolicy,
) -> (Sender<T>, Receiver<T>) {
    let (tx, rx) = crate::packet_queue::lossy_channel(items, bytes, policy);
    (Sender(tx), Receiver(rx))
}
pub use crate::protocols::udp::bench_support::RawPeer;
pub use crate::server::bench_support::Fixture as ServerFixture;

#[derive(Clone, Copy, Debug)]
pub enum Scenario {
    Packets,
    Interleaved,
}
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delivery {
    pub frames: usize,
    pub packets: usize,
    pub established: usize,
    pub closed: usize,
    pub last_packet_frame: usize,
    pub last_close_frame: usize,
}
