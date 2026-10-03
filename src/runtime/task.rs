use std::future::Future;
use tokio_util::sync::CancellationToken;

// Shared by native Tokio workers and the browser event loop. No Send bound:
// browser transports may await JavaScript futures on their current thread.
pub(super) async fn until_cancelled<F: Future<Output = ()>>(stop: CancellationToken, future: F) {
    tokio::select! {
        biased;
        () = stop.cancelled() => {},
        () = future => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{pin_mut, poll};
    use std::cell::Cell;
    use std::rc::Rc;

    struct OnDrop(Rc<Cell<bool>>);

    impl Drop for OnDrop {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    #[test]
    fn cancellation_drops_a_pending_non_send_future() {
        let stop = CancellationToken::new();
        let dropped = Rc::new(Cell::new(false));
        let guard = OnDrop(Rc::clone(&dropped));
        futures::executor::block_on(async {
            let task = until_cancelled(stop.clone(), async move {
                let _guard = guard;
                std::future::pending::<()>().await;
            });
            pin_mut!(task);
            assert!(poll!(&mut task).is_pending());
            assert!(!dropped.get());
            stop.cancel();
            task.await;
            assert!(dropped.get());
        });
    }

    #[test]
    fn cancellation_before_first_poll_drops_future_without_running_it() {
        let stop = CancellationToken::new();
        stop.cancel();
        let ran = Cell::new(false);
        let dropped = Rc::new(Cell::new(false));
        let guard = OnDrop(Rc::clone(&dropped));
        futures::executor::block_on(until_cancelled(stop, async {
            let _guard = guard;
            ran.set(true);
        }));
        assert!(!ran.get());
        assert!(dropped.get());
    }
}
