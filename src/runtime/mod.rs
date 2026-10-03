//! Tokio execution shared by the network endpoints in one Bevy app.

mod task;

use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::{mpsc, Arc, Mutex};
use std::task::{Context, Wake, Waker};
use std::time::{Duration, Instant};

use bevy::app::{App, AppExit, Last, Plugin, Startup};
use bevy::ecs::message::MessageReader;
use bevy::log;
use bevy::prelude::{IntoScheduleConfigs, Res, Resource, SystemSet};
use futures::FutureExt;
use tokio::runtime::{Handle, RuntimeFlavor};
use tokio::task::LocalSet;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// Selects who owns the Tokio runtime used by this app's network endpoints.
pub enum NetworkRuntimeMode {
    /// Build a runtime for this app with the given number of worker threads.
    Owned { worker_threads: NonZeroUsize },
    /// Use a caller-owned multi-thread runtime with the drivers required by the transports.
    External(Handle),
}

/// Configure Tokio execution before the first app update.
#[derive(Resource)]
pub struct NetworkRuntimeSettings {
    /// Runtime ownership and worker configuration.
    pub mode: NetworkRuntimeMode,
    /// Maximum time to wait for network shutdown.
    pub shutdown_timeout: Duration,
}

impl Default for NetworkRuntimeSettings {
    fn default() -> Self {
        Self {
            mode: NetworkRuntimeMode::Owned {
                worker_threads: NonZeroUsize::new(2).unwrap_or(NonZeroUsize::MIN),
            },
            shutdown_timeout: Duration::from_secs(1),
        }
    }
}

type LocalCommand = Box<dyn FnOnce() -> std::pin::Pin<Box<dyn Future<Output = ()>>> + Send>;

struct Inner {
    handle: Handle,
    stop: CancellationToken,
    tasks: TaskTracker,
    // Keeps the command channel open even in client-only builds.
    local_tx: tokio::sync::mpsc::UnboundedSender<LocalCommand>,
}

tokio::task_local! {
    static CURRENT: Arc<Inner>;
}

// Also protects the failed-thread-spawn path: dropping a Runtime directly in
// an async caller would panic while trying to wait for its blocking pool.
struct OwnedRuntime(Option<tokio::runtime::Runtime>);

impl Drop for OwnedRuntime {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

struct ThreadWake(std::thread::Thread);

impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

// Shutdown must also work when an external runtime has no timer driver or all
// of its workers are blocked. TaskTracker's wait future needs only a waker.
fn wait_for_tasks(tasks: &TaskTracker, timeout: Duration) -> bool {
    let started = Instant::now();
    let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut wait = std::pin::pin!(tasks.wait());
    loop {
        if wait.as_mut().poll(&mut context).is_ready() {
            return true;
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return false;
        }
        std::thread::park_timeout(remaining);
    }
}

async fn run_task<F>(inner: Arc<Inner>, future: F)
where
    F: Future<Output = ()>,
{
    let stop = inner.stop.clone();
    let task = task::until_cancelled(stop, future);
    if CURRENT
        .scope(inner, std::panic::AssertUnwindSafe(task).catch_unwind())
        .await
        .is_err()
    {
        log::error!("Network task panicked");
    }
}

fn spawn_on<F>(inner: &Arc<Inner>, future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    if !inner.stop.is_cancelled() {
        inner
            .tasks
            .spawn_on(run_task(Arc::clone(inner), future), &inner.handle);
    }
}

/// Spawn a network child task and include it in this app's shutdown.
#[cfg(any(feature = "client", feature = "server", test))]
pub(crate) fn spawn<F>(future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    CURRENT.with(|inner| spawn_on(inner, future));
}

pub(crate) fn connection_token() -> CancellationToken {
    CURRENT
        .try_with(|inner| inner.stop.child_token())
        .unwrap_or_default()
}

/// App-scoped executor installed by [`NetworkRuntimePlugin`] at startup.
///
/// Tasks are cancelled on `AppExit` or drop. An external runtime stays owned by
/// its caller and must outlive this resource.
#[derive(Resource)]
pub struct NetworkRuntime {
    inner: Option<Arc<Inner>>,
    thread: Option<std::thread::JoinHandle<()>>,
    done: Option<Mutex<mpsc::Receiver<()>>>,
    timeout: Duration,
}

impl NetworkRuntime {
    /// Whether startup succeeded and shutdown has not begun.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.inner
            .as_ref()
            .is_some_and(|inner| !inner.stop.is_cancelled() && !inner.local_tx.is_closed())
    }

    fn new(settings: &NetworkRuntimeSettings) -> Self {
        let (handle, owned) = match &settings.mode {
            NetworkRuntimeMode::Owned { worker_threads } => {
                match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(worker_threads.get())
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => (runtime.handle().clone(), Some(runtime)),
                    Err(error) => {
                        log::error!("Failed to create network runtime: {error}");
                        return Self::unavailable(settings.shutdown_timeout);
                    }
                }
            }
            NetworkRuntimeMode::External(handle) => {
                if handle.runtime_flavor() != RuntimeFlavor::MultiThread {
                    log::error!("External network runtime must use Tokio multi_thread flavor");
                    return Self::unavailable(settings.shutdown_timeout);
                }
                (handle.clone(), None)
            }
        };
        let mut owned = OwnedRuntime(owned);
        let (local_tx, mut local_rx) = tokio::sync::mpsc::unbounded_channel::<LocalCommand>();
        let inner = Arc::new(Inner {
            handle: handle.clone(),
            stop: CancellationToken::new(),
            tasks: TaskTracker::new(),
            local_tx,
        });
        let thread_inner = Arc::clone(&inner);
        let timeout = settings.shutdown_timeout;
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let thread = match std::thread::Builder::new()
            .name("bevy_slinet-local".into())
            .spawn(move || {
                let local = LocalSet::new();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    handle.block_on(local.run_until(async {
                        loop {
                            tokio::select! {
                                () = thread_inner.stop.cancelled() => break,
                                command = local_rx.recv() => {
                                    let Some(command) = command else { break };
                                    let child = Arc::clone(&thread_inner);
                                    thread_inner.tasks.spawn_local(run_task(child, async move {
                                        command().await;
                                    }));
                                }
                            }
                        }
                    }));
                }));
                if result.is_err() {
                    log::error!("Network local executor stopped unexpectedly");
                }
                thread_inner.stop.cancel();
                thread_inner.tasks.close();
                drop(local);
                // Release queued listener factories and their captured channels too.
                drop(local_rx);
                let started = Instant::now();
                // This thread can wait even when the Bevy App is dropped from
                // inside another Tokio runtime. Only this app's tasks are tracked.
                if !wait_for_tasks(&thread_inner.tasks, timeout) {
                    log::error!("Network tasks did not stop before the shutdown timeout");
                }
                if let Some(runtime) = owned.0.take() {
                    runtime.shutdown_timeout(timeout.saturating_sub(started.elapsed()));
                }
                let _ = done_tx.send(());
            }) {
            Ok(thread) => thread,
            Err(error) => {
                log::error!("Failed to create network local thread: {error}");
                // OwnedRuntime shuts down without blocking if the closure is dropped.
                return Self::unavailable(timeout);
            }
        };
        Self {
            inner: Some(inner),
            thread: Some(thread),
            done: Some(Mutex::new(done_rx)),
            timeout,
        }
    }

    const fn unavailable(timeout: Duration) -> Self {
        Self {
            inner: None,
            thread: None,
            done: None,
            timeout,
        }
    }

    /// Runs a task on the shared worker pool until completion or app shutdown.
    /// An unavailable runtime drops the future without polling it.
    pub fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        if let Some(inner) = &self.inner {
            spawn_on(inner, future);
        }
    }

    /// Builds and runs a possibly non-Send future on the runtime's local thread.
    /// The factory crosses threads; create non-Send state inside it.
    /// An unavailable runtime drops the factory without invoking it.
    pub fn spawn_local<F, Fut>(&self, make_future: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        if let Some(inner) = &self.inner {
            if inner
                .local_tx
                .send(Box::new(move || Box::pin(make_future())))
                .is_err()
            {
                log::error!("Network local executor is unavailable");
            }
        }
    }

    fn shutdown(&mut self) {
        if let Some(inner) = self.inner.take() {
            inner.stop.cancel();
            inner.tasks.close();
        }
        if let Some(mut done) = self.done.take() {
            let result = done
                .get_mut()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .recv_timeout(self.timeout);
            if matches!(result, Err(mpsc::RecvTimeoutError::Timeout)) {
                log::error!("Network shutdown timed out");
            }
            if let Some(thread) = self.thread.take() {
                if (!matches!(result, Err(mpsc::RecvTimeoutError::Timeout)) || thread.is_finished())
                    && thread.join().is_err()
                {
                    log::error!("Network local thread panicked");
                }
            }
        }
    }
}

impl Drop for NetworkRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Initializes [`NetworkRuntime`] in `Startup`.
/// Configure settings before this set and start custom tasks after it.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RuntimeSetup;

/// Installs the app-scoped executor and cancels its tasks on `AppExit` or drop.
///
/// Client and server plugins add it automatically; custom plugins can add it
/// after checking `app.is_plugin_added::<NetworkRuntimePlugin>()`.
pub struct NetworkRuntimePlugin;

impl Plugin for NetworkRuntimePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<NetworkRuntimeSettings>()
            .add_systems(Startup, initialize.in_set(RuntimeSetup))
            .add_systems(Last, stop_on_exit);
    }
}

/// Install the shared executor once for all endpoint plugins in one app.
#[cfg(any(feature = "client", feature = "server", test))]
pub(crate) fn register(app: &mut App) {
    if !app.is_plugin_added::<NetworkRuntimePlugin>() {
        app.add_plugins(NetworkRuntimePlugin);
    }
}

fn initialize(mut commands: bevy::prelude::Commands, settings: Res<NetworkRuntimeSettings>) {
    commands.insert_resource(NetworkRuntime::new(&settings));
}

fn stop_on_exit(
    mut exits: MessageReader<AppExit>,
    mut runtime: Option<bevy::prelude::ResMut<NetworkRuntime>>,
) {
    if exits.read().next().is_some() {
        if let Some(runtime) = runtime.as_mut() {
            runtime.shutdown();
        }
    }
}

#[cfg(test)]
mod tests;
