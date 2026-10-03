//! App-scoped network tasks on the JavaScript event loop.
//!
//! No Tokio runtime or worker threads are created. Custom transports must use
//! browser APIs for I/O and timers. Native runtime settings do not apply here.

mod task;

use std::future::Future;

use bevy::app::{App, AppExit, Last, Plugin, Startup};
use bevy::ecs::message::MessageReader;
use bevy::prelude::{Commands, IntoScheduleConfigs, ResMut, Resource, SystemSet};
use tokio_util::sync::CancellationToken;

tokio::task_local! {
    static CURRENT: CancellationToken;
}

fn spawn_on<F: Future<Output = ()> + 'static>(stop: &CancellationToken, future: F) {
    if stop.is_cancelled() {
        return;
    }
    let stop = stop.clone();
    wasm_bindgen_futures::spawn_local(
        CURRENT.scope(stop.clone(), task::until_cancelled(stop, future)),
    );
}

pub(crate) fn spawn<F: Future<Output = ()> + 'static>(future: F) {
    CURRENT.with(|stop| spawn_on(stop, future));
}

pub(crate) fn connection_token() -> CancellationToken {
    CURRENT
        .try_with(CancellationToken::child_token)
        .unwrap_or_default()
}

/// Network executor backed by the JavaScript event loop.
///
/// `AppExit` and drop cancel this app's tasks. Futures are released on a
/// subsequent event-loop turn: shutdown never blocks the browser thread.
#[derive(Resource, Default)]
pub struct NetworkRuntime {
    stop: CancellationToken,
}

impl NetworkRuntime {
    /// Whether shutdown has not begun.
    #[must_use]
    pub fn is_available(&self) -> bool {
        !self.stop.is_cancelled()
    }

    #[cfg(feature = "client")]
    pub(crate) fn spawn<F: Future<Output = ()> + 'static>(&self, future: F) {
        spawn_on(&self.stop, future);
    }

    #[cfg(feature = "server")]
    pub(crate) fn spawn_local<F, Fut>(&self, make_future: F)
    where
        F: FnOnce() -> Fut + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        spawn_on(&self.stop, async move { make_future().await });
    }
}

impl Drop for NetworkRuntime {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct RuntimeSetup;

struct RuntimePlugin;

impl Plugin for RuntimePlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, initialize.in_set(RuntimeSetup))
            .add_systems(Last, stop_on_exit);
    }
}

pub(crate) fn register(app: &mut App) {
    if !app.is_plugin_added::<RuntimePlugin>() {
        app.add_plugins(RuntimePlugin);
    }
}

fn initialize(mut commands: Commands) {
    commands.init_resource::<NetworkRuntime>();
}

fn stop_on_exit(mut exits: MessageReader<AppExit>, runtime: Option<ResMut<NetworkRuntime>>) {
    if exits.read().next().is_some() {
        if let Some(runtime) = runtime {
            runtime.stop.cancel();
        }
    }
}
