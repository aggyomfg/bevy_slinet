#![cfg(not(target_family = "wasm"))]
#![allow(
    clippy::unwrap_used,
    reason = "Tests fail on unexpected runtime errors"
)]

use std::cell::Cell;
use std::rc::Rc;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::time::Duration;

use bevy::prelude::*;
use bevy_slinet::runtime::{
    NetworkRuntime, NetworkRuntimeMode, NetworkRuntimePlugin, NetworkRuntimeSettings, RuntimeSetup,
};

struct MarkDropped(Arc<AtomicBool>);

impl Drop for MarkDropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[derive(Resource)]
struct CustomTasks {
    ready: mpsc::Sender<bool>,
    worker_dropped: Arc<AtomicBool>,
    local_dropped: Arc<AtomicBool>,
}

struct CustomNetworkPlugin;

impl Plugin for CustomNetworkPlugin {
    fn build(&self, app: &mut App) {
        if !app.is_plugin_added::<NetworkRuntimePlugin>() {
            app.add_plugins(NetworkRuntimePlugin);
        }
        app.add_systems(Startup, start_custom_tasks.after(RuntimeSetup));
    }
}

fn start_custom_tasks(runtime: Res<NetworkRuntime>, tasks: Res<CustomTasks>) {
    assert!(runtime.is_available());
    let ready = tasks.ready.clone();
    let guard = MarkDropped(Arc::clone(&tasks.worker_dropped));
    runtime.spawn(async move {
        let _guard = guard;
        ready
            .send(std::thread::current().name() == Some("custom-network-worker"))
            .unwrap();
        std::future::pending::<()>().await;
    });
    let ready = tasks.ready.clone();
    let guard = MarkDropped(Arc::clone(&tasks.local_dropped));
    runtime.spawn_local(move || async move {
        let _guard = guard;
        let non_send = Rc::new(Cell::new(false));
        tokio::task::yield_now().await;
        non_send.set(true);
        ready.send(non_send.get()).unwrap();
        std::future::pending::<()>().await;
    });
}

fn external_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("custom-network-worker")
        .enable_all()
        .build()
        .unwrap()
}

fn custom_tasks() -> (CustomTasks, mpsc::Receiver<bool>) {
    let (ready, receiver) = mpsc::channel();
    (
        CustomTasks {
            ready,
            worker_dropped: Arc::new(AtomicBool::new(false)),
            local_dropped: Arc::new(AtomicBool::new(false)),
        },
        receiver,
    )
}

#[test]
fn custom_plugin_preserves_settings_and_cancels_its_tasks_on_drop() {
    let external = external_runtime();
    let (tasks, ready) = custom_tasks();
    let worker_dropped = Arc::clone(&tasks.worker_dropped);
    let local_dropped = Arc::clone(&tasks.local_dropped);
    let mut app = App::new();
    app.insert_resource(NetworkRuntimeSettings {
        mode: NetworkRuntimeMode::External(external.handle().clone()),
        shutdown_timeout: Duration::from_millis(800),
    })
    .insert_resource(tasks)
    .add_plugins((NetworkRuntimePlugin, CustomNetworkPlugin));
    assert_eq!(
        app.world()
            .resource::<NetworkRuntimeSettings>()
            .shutdown_timeout,
        Duration::from_millis(800)
    );
    app.update();
    assert!(ready.recv_timeout(Duration::from_secs(2)).unwrap());
    assert!(ready.recv_timeout(Duration::from_secs(2)).unwrap());
    drop(app);
    assert!(worker_dropped.load(Ordering::SeqCst));
    assert!(local_dropped.load(Ordering::SeqCst));
    assert_eq!(
        external.block_on(async { tokio::spawn(async { 7 }).await.unwrap() }),
        7
    );
}

#[test]
fn public_setup_set_applies_startup_settings_and_exit_cancels_custom_tasks() {
    let external = external_runtime();
    let handle = external.handle().clone();
    let (tasks, ready) = custom_tasks();
    let worker_dropped = Arc::clone(&tasks.worker_dropped);
    let local_dropped = Arc::clone(&tasks.local_dropped);
    let mut app = App::new();
    app.insert_resource(tasks)
        .add_plugins(CustomNetworkPlugin)
        .add_systems(
            Startup,
            (move |mut commands: Commands| {
                commands.insert_resource(NetworkRuntimeSettings {
                    mode: NetworkRuntimeMode::External(handle.clone()),
                    ..Default::default()
                });
            })
            .before(RuntimeSetup),
        );
    app.update();
    assert!(ready.recv_timeout(Duration::from_secs(2)).unwrap());
    assert!(ready.recv_timeout(Duration::from_secs(2)).unwrap());
    app.world_mut().write_message(AppExit::Success);
    app.update();
    assert!(worker_dropped.load(Ordering::SeqCst));
    assert!(local_dropped.load(Ordering::SeqCst));
    let runtime = app.world().resource::<NetworkRuntime>();
    assert!(!runtime.is_available());

    let dropped = Arc::new(AtomicBool::new(false));
    let guard = MarkDropped(Arc::clone(&dropped));
    runtime.spawn(async move {
        let _guard = guard;
        std::future::pending::<()>().await;
    });
    assert!(dropped.load(Ordering::SeqCst));
    let dropped = Arc::new(AtomicBool::new(false));
    let guard = MarkDropped(Arc::clone(&dropped));
    runtime.spawn_local(move || async move {
        let _guard = guard;
        std::future::pending::<()>().await;
    });
    assert!(dropped.load(Ordering::SeqCst));
}
