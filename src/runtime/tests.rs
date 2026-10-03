#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use bevy::prelude::Messages;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};

#[test]
fn owned_runtime_runs_send_and_local_tasks_and_stops_on_exit() {
    let mut app = App::new();
    register(&mut app);
    app.update();
    let runtime = app.world().resource::<NetworkRuntime>();
    assert!(runtime.is_available());
    let (tx, rx) = mpsc::sync_channel(2);
    let send_tx = tx.clone();
    runtime.spawn(async move { send_tx.send(1).unwrap() });
    runtime.spawn_local(move || async move {
        let value = Rc::new(Cell::new(1));
        value.set(value.get() + 1);
        tx.send(value.get()).unwrap();
    });
    let mut values = [
        rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        rx.recv_timeout(Duration::from_secs(2)).unwrap(),
    ];
    values.sort_unstable();
    assert_eq!(values, [1, 2]);
    app.world_mut()
        .resource_mut::<Messages<AppExit>>()
        .write(AppExit::Success);
    app.update();
    assert!(!app.world().resource::<NetworkRuntime>().is_available());
}

#[test]
fn external_runtime_survives_app_and_rejects_current_thread() {
    let external = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut app = App::new();
    register(&mut app);
    app.insert_resource(NetworkRuntimeSettings {
        mode: NetworkRuntimeMode::External(external.handle().clone()),
        ..Default::default()
    });
    app.update();
    assert!(app.world().resource::<NetworkRuntime>().is_available());
    let alive = Arc::new(AtomicBool::new(false));
    let mark = Arc::clone(&alive);
    external.block_on(async move {
        tokio::spawn(async move { mark.store(true, Ordering::SeqCst) })
            .await
            .unwrap();
    });
    drop(app);
    assert!(alive.load(Ordering::SeqCst));
    external.block_on(async { tokio::task::yield_now().await });

    let current = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut app = App::new();
    register(&mut app);
    app.insert_resource(NetworkRuntimeSettings {
        mode: NetworkRuntimeMode::External(current.handle().clone()),
        ..Default::default()
    });
    app.update();
    assert!(!app.world().resource::<NetworkRuntime>().is_available());
}

struct MarkDropped(Arc<AtomicBool>);

impl Drop for MarkDropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[test]
fn shutdown_waits_for_child_and_local_futures_without_stopping_other_apps() {
    let external = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let settings = NetworkRuntimeSettings {
        mode: NetworkRuntimeMode::External(external.handle().clone()),
        ..Default::default()
    };
    let mut first = NetworkRuntime::new(&settings);
    let second = NetworkRuntime::new(&settings);
    let child_dropped = Arc::new(AtomicBool::new(false));
    let local_dropped = Arc::new(AtomicBool::new(false));
    let other_dropped = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = mpsc::channel();
    let guard = MarkDropped(Arc::clone(&child_dropped));
    let child_tx = ready_tx.clone();
    first.spawn(async move {
        spawn(async move {
            let _guard = guard;
            child_tx.send(connection_token()).unwrap();
            std::future::pending::<()>().await;
        });
    });
    let guard = MarkDropped(Arc::clone(&local_dropped));
    let local_tx = ready_tx;
    first.spawn_local(move || async move {
        let _guard = guard;
        let _non_send = Rc::new(Cell::new(0));
        local_tx.send(connection_token()).unwrap();
        std::future::pending::<()>().await;
    });
    let guard = MarkDropped(Arc::clone(&other_dropped));
    let (other_tx, other_rx) = mpsc::channel();
    second.spawn(async move {
        let _guard = guard;
        other_tx.send(connection_token()).unwrap();
        std::future::pending::<()>().await;
    });
    let tokens = [
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
    ];
    let other_token = other_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    first.shutdown();
    first.shutdown(); // Idempotent, including the subsequent Drop.
    assert!(tokens.iter().all(CancellationToken::is_cancelled));
    assert!(child_dropped.load(Ordering::SeqCst));
    assert!(local_dropped.load(Ordering::SeqCst));
    assert!(!other_token.is_cancelled());
    assert!(!other_dropped.load(Ordering::SeqCst));
    let (tx, rx) = mpsc::channel();
    second.spawn(async move { tx.send(()).unwrap() });
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    drop(second);
    assert!(other_dropped.load(Ordering::SeqCst));
}

#[test]
fn child_spawned_during_shutdown_is_never_polled() {
    let mut runtime = NetworkRuntime::new(&NetworkRuntimeSettings::default());
    let (tx, rx) = mpsc::channel();
    let polled = Arc::new(AtomicBool::new(false));
    let child_polled = Arc::clone(&polled);
    runtime.spawn(async move {
        // Simulate cancellation arriving while this parent is being polled.
        CURRENT.with(|inner| inner.stop.cancel());
        spawn(async move { child_polled.store(true, Ordering::SeqCst) });
        tx.send(()).unwrap();
    });
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    runtime.shutdown();
    assert!(!polled.load(Ordering::SeqCst));
}

#[test]
fn shutdown_timeout_bounds_wait_for_blocking_user_code() {
    let external = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let mut runtime = NetworkRuntime::new(&NetworkRuntimeSettings {
        mode: NetworkRuntimeMode::External(external.handle().clone()),
        shutdown_timeout: Duration::from_millis(20),
    });
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    runtime.spawn(async move {
        ready_tx.send(()).unwrap();
        // Deliberately simulate a blocking custom protocol callback.
        release_rx.recv().unwrap();
    });
    ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let started = std::time::Instant::now();
    runtime.shutdown();
    let elapsed = started.elapsed();
    release_tx.send(()).unwrap();
    assert!(elapsed < Duration::from_secs(2));
    assert!(!runtime.is_available());
}

#[test]
fn external_runtime_without_drivers_shuts_down_cleanly() {
    let external = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap();
    let mut runtime = NetworkRuntime::new(&NetworkRuntimeSettings {
        mode: NetworkRuntimeMode::External(external.handle().clone()),
        ..Default::default()
    });
    let (tx, rx) = mpsc::channel();
    let dropped = Arc::new(AtomicBool::new(false));
    let guard = MarkDropped(Arc::clone(&dropped));
    runtime.spawn(async move {
        let _guard = guard;
        tx.send(()).unwrap();
        std::future::pending::<()>().await;
    });
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let thread = runtime.thread.take().unwrap();
    runtime.shutdown();
    assert!(thread.join().is_ok());
    assert!(dropped.load(Ordering::SeqCst));
}

#[test]
fn stopped_external_runtime_does_not_panic_during_cleanup() {
    let external = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let mut runtime = NetworkRuntime::new(&NetworkRuntimeSettings {
        mode: NetworkRuntimeMode::External(external.handle().clone()),
        ..Default::default()
    });
    drop(external);
    let thread = runtime.thread.take().unwrap();
    runtime.shutdown();
    assert!(thread.join().is_ok());
}

#[test]
fn coordinator_timeout_does_not_depend_on_external_workers() {
    let external = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let mut runtime = NetworkRuntime::new(&NetworkRuntimeSettings {
        mode: NetworkRuntimeMode::External(external.handle().clone()),
        shutdown_timeout: Duration::from_millis(20),
    });
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    runtime.spawn(async move {
        ready_tx.send(()).unwrap();
        // Stalls the only worker, including Tokio's timer driver.
        let _ = release_rx.recv();
    });
    ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let done = runtime.done.take().unwrap().into_inner().unwrap();
    let thread = runtime.thread.take().unwrap();
    runtime.shutdown();
    let completed = done.recv_timeout(Duration::from_secs(2));
    // Unblock the worker even when the assertion fails.
    drop(release_tx);
    assert!(completed.is_ok());
    assert!(thread.join().is_ok());
}

#[test]
fn owned_runtime_can_be_dropped_inside_another_runtime() {
    let outer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    outer.block_on(async {
        // Models dropping the thread closure when OS thread creation fails.
        drop(OwnedRuntime(Some(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap(),
        )));
        let mut app = App::new();
        register(&mut app);
        app.update();
        drop(app);
    });
}
