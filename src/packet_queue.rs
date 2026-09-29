//! A bounded queue that can discard the oldest item when configured to do so.

// Public only through the unstable benchmark API.
#![cfg_attr(
    feature = "bench-internals",
    allow(clippy::missing_errors_doc, clippy::must_use_candidate)
)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::mpsc::error::{TryRecvError, TrySendError};
use tokio::sync::Notify;

use crate::connection::OverflowPolicy;

struct State<T> {
    items: VecDeque<(T, usize)>,
    bytes: usize,
    senders: usize,
    receiver_open: bool,
}

struct Shared<T> {
    state: Mutex<State<T>>,
    changed: Notify,
    space: Notify,
    max_items: usize,
    max_bytes: usize,
    policy: OverflowPolicy,
}

impl<T> Shared<T> {
    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The sending side of a bounded lossy queue.
pub struct LossySender<T>(Arc<Shared<T>>);

impl<T> Clone for LossySender<T> {
    fn clone(&self) -> Self {
        self.0.lock().senders += 1;
        Self(Arc::clone(&self.0))
    }
}

impl<T> Drop for LossySender<T> {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.senders -= 1;
        if state.senders == 0 {
            drop(state);
            self.0.changed.notify_waiters();
        }
    }
}

impl<T> LossySender<T> {
    /// Reports whether the receiver has closed.
    #[cfg(any(feature = "client", feature = "server"))]
    pub fn is_closed(&self) -> bool {
        !self.0.lock().receiver_open
    }

    /// Samples outgoing packet occupancy under the queue lock.
    #[cfg(any(feature = "client", feature = "server"))]
    pub fn snapshot(&self) -> crate::connection::QueueSnapshot {
        crate::connection::QueueSnapshot {
            queued: self.0.lock().items.len(),
            capacity: self.0.max_items,
        }
    }

    /// Tries to enqueue an item with its exact byte weight, returning any evicted items.
    /// An individually oversized item never evicts an existing item.
    pub fn try_send(&self, value: T, bytes: usize) -> Result<Vec<T>, TrySendError<T>> {
        self.try_send_where(value, bytes, |_| true)
    }

    /// Tries to enqueue while protecting items for which `can_evict` returns false.
    pub fn try_send_where(
        &self,
        value: T,
        bytes: usize,
        can_evict: impl Fn(&T) -> bool,
    ) -> Result<Vec<T>, TrySendError<T>> {
        let mut state = self.0.lock();
        if !state.receiver_open {
            return Err(TrySendError::Closed(value));
        }
        if self.0.max_items == 0 || bytes > self.0.max_bytes {
            return Err(TrySendError::Full(value));
        }
        let mut evicted = Vec::new();
        if self.0.policy == OverflowPolicy::DropOldest {
            let mut remaining_items = state.items.len();
            let mut remaining_bytes = state.bytes;
            for (old, old_bytes) in &state.items {
                if remaining_items < self.0.max_items && remaining_bytes <= self.0.max_bytes - bytes
                {
                    break;
                }
                if !can_evict(old) {
                    return Err(TrySendError::Full(value));
                }
                remaining_items -= 1;
                remaining_bytes -= old_bytes;
            }
            while state.items.len() >= self.0.max_items || state.bytes > self.0.max_bytes - bytes {
                if let Some((old, old_bytes)) = state.items.pop_front() {
                    state.bytes -= old_bytes;
                    evicted.push(old);
                } else {
                    break;
                }
            }
        }
        if state.items.len() >= self.0.max_items || state.bytes > self.0.max_bytes - bytes {
            return Err(TrySendError::Full(value));
        }
        state.bytes += bytes;
        state.items.push_back((value, bytes));
        drop(state);
        self.0.changed.notify_one();
        Ok(evicted)
    }

    /// Waits for capacity and enqueues without discarding an existing item.
    #[cfg(any(feature = "client", feature = "server", test))]
    pub async fn send(&self, value: T, bytes: usize) -> Result<(), TrySendError<T>> {
        if self.0.max_items == 0 || bytes > self.0.max_bytes {
            return Err(TrySendError::Full(value));
        }
        let shared = Arc::clone(&self.0);
        loop {
            let notified = shared.space.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let ready = {
                let state = shared.lock();
                if !state.receiver_open {
                    return Err(TrySendError::Closed(value));
                }
                state.items.len() < shared.max_items && state.bytes <= shared.max_bytes - bytes
            };
            if ready {
                let mut state = shared.lock();
                if !state.receiver_open {
                    return Err(TrySendError::Closed(value));
                }
                if state.items.len() < shared.max_items && state.bytes <= shared.max_bytes - bytes {
                    state.bytes += bytes;
                    state.items.push_back((value, bytes));
                    drop(state);
                    shared.changed.notify_one();
                    return Ok(());
                }
            }
            notified.await;
        }
    }

    /// Sum of byte weights assigned to queued items.
    #[cfg(test)]
    pub fn queued_bytes(&self) -> usize {
        self.0.lock().bytes
    }
}

/// The single receiving side of a bounded lossy queue.
pub struct LossyReceiver<T>(Arc<Shared<T>>);

impl<T> Drop for LossyReceiver<T> {
    fn drop(&mut self) {
        drop(self.close_and_drain());
    }
}

impl<T> LossyReceiver<T> {
    /// Closes the receiver and returns queued items in FIFO order.
    #[cfg_attr(
        not(feature = "bench-internals"),
        expect(
            clippy::needless_pass_by_ref_mut,
            reason = "Only the single receiver may close and drain"
        )
    )]
    pub fn close_and_drain(&mut self) -> Vec<T> {
        let items = {
            let mut state = self.0.lock();
            state.receiver_open = false;
            state.bytes = 0;
            std::mem::take(&mut state.items)
        };
        self.0.changed.notify_waiters();
        self.0.space.notify_waiters();
        items.into_iter().map(|(item, _)| item).collect()
    }

    /// Sum of byte weights assigned to queued items.
    #[cfg(all(test, feature = "protocol_udp"))]
    pub fn queued_bytes(&self) -> usize {
        self.0.lock().bytes
    }

    /// Receives the next item, or `None` after all senders close and the queue drains.
    pub async fn recv(&mut self) -> Option<T> {
        let shared = Arc::clone(&self.0);
        loop {
            let notified = shared.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.try_recv() {
                Ok(value) => return Some(value),
                Err(TryRecvError::Disconnected) => return None,
                Err(TryRecvError::Empty) => notified.await,
            }
        }
    }

    /// Receives the next queued item without waiting.
    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        self.try_recv_if(|_| true)
    }

    /// Receives the front item only when `ready` accepts it, leaving it queued otherwise.
    #[cfg_attr(
        not(feature = "bench-internals"),
        expect(
            clippy::needless_pass_by_ref_mut,
            reason = "Only the single receiver may remove queue items"
        )
    )]
    pub fn try_recv_if(&mut self, ready: impl Fn(&T) -> bool) -> Result<T, TryRecvError> {
        let mut state = self.0.lock();
        if state.items.front().is_some_and(|(item, _)| ready(item)) {
            if let Some((item, bytes)) = state.items.pop_front() {
                state.bytes -= bytes;
                drop(state);
                self.0.space.notify_one();
                return Ok(item);
            }
        }
        if state.items.is_empty() && (state.senders == 0 || !state.receiver_open) {
            Err(TryRecvError::Disconnected)
        } else {
            Err(TryRecvError::Empty)
        }
    }
}

/// Creates a single-receiver bounded queue with packet and byte limits.
pub fn lossy_channel<T>(
    max_items: usize,
    max_bytes: usize,
    policy: OverflowPolicy,
) -> (LossySender<T>, LossyReceiver<T>) {
    let shared = Arc::new(Shared {
        state: Mutex::new(State {
            items: VecDeque::new(),
            bytes: 0,
            senders: 1,
            receiver_open: true,
        }),
        changed: Notify::new(),
        space: Notify::new(),
        max_items,
        max_bytes,
        policy,
    });
    (LossySender(Arc::clone(&shared)), LossyReceiver(shared))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn oldest_eviction_accounts_bytes_and_preserves_fifo() {
        let (tx, mut rx) = lossy_channel(2, 5, OverflowPolicy::DropOldest);
        assert!(tx.try_send(1, 2).unwrap().is_empty());
        assert!(tx.try_send(2, 3).unwrap().is_empty());
        assert!(matches!(tx.try_send(3, 6), Err(TrySendError::Full(3))));
        assert_eq!(tx.queued_bytes(), 5);
        assert_eq!(tx.try_send(4, 4).unwrap(), vec![1, 2]);
        assert_eq!(tx.queued_bytes(), 4);
        assert_eq!(rx.recv().await, Some(4));
        drop(tx);
        assert_eq!(rx.recv().await, None);
    }

    #[tokio::test]
    async fn dropping_receiver_closes_all_senders() {
        let (tx, rx) = lossy_channel(1, 1, OverflowPolicy::DropNewest);
        let cloned = tx.clone();
        drop(rx);
        assert!(matches!(tx.try_send(1, 1), Err(TrySendError::Closed(1))));
        assert!(matches!(
            cloned.try_send(2, 1),
            Err(TrySendError::Closed(2))
        ));
    }

    #[tokio::test]
    async fn waiting_receiver_wakes_when_last_sender_closes() {
        let (tx, mut rx) = lossy_channel::<u8>(1, 1, OverflowPolicy::DropNewest);
        let pending = tokio::spawn(async move { rx.recv().await });
        tokio::task::yield_now().await;
        drop(tx);
        assert_eq!(pending.await.unwrap(), None);
    }

    #[tokio::test]
    async fn waiting_sender_wakes_when_space_frees_or_receiver_closes() {
        let (tx, mut rx) = lossy_channel(1, 1, OverflowPolicy::DropNewest);
        tx.try_send(1, 1).unwrap();
        let waiting = tokio::spawn({
            let tx = tx.clone();
            async move { tx.send(2, 1).await }
        });
        tokio::task::yield_now().await;
        assert_eq!(rx.recv().await, Some(1));
        assert!(waiting.await.unwrap().is_ok());
        assert_eq!(rx.recv().await, Some(2));
        tx.try_send(3, 1).unwrap();
        let waiting = tokio::spawn({
            let tx = tx.clone();
            async move { tx.send(4, 1).await }
        });
        tokio::task::yield_now().await;
        drop(rx);
        assert!(matches!(
            waiting.await.unwrap(),
            Err(TrySendError::Closed(4))
        ));
    }

    #[tokio::test]
    async fn deferred_front_stays_inside_capacity_and_is_evicted_first() {
        let (tx, mut rx) = lossy_channel(1, 1, OverflowPolicy::DropOldest);
        tx.try_send(1, 1).unwrap();
        assert!(matches!(
            rx.try_recv_if(|_| false),
            Err(TryRecvError::Empty)
        ));
        assert_eq!(tx.try_send(2, 1).unwrap(), vec![1]);
        assert_eq!(rx.recv().await, Some(2));
    }
}
