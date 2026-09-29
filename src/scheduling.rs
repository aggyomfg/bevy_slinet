//! Public scheduling labels and shared app-wide limit synchronization.
use bevy::prelude::SystemSet;
use std::{
    fmt,
    hash::{Hash, Hasher},
    marker::PhantomData,
};

/// Scheduling phases for one endpoint (role and config).
///
/// Use `client::ClientSystems<Config>` or `server::ServerSystems<Config>`.
/// The plugin installs these sets automatically; applications use `.before(...)`,
/// `.after(...)` or `configure_sets` in the indicated schedule.
/// Ordering does not cross schedules. With Bevy's default schedule settings,
/// `.after(...)` also applies pending commands, making observer effects visible.
#[derive(SystemSet)]
pub struct NetworkSystems<Endpoint: Send + Sync + 'static> {
    phase: Phase,
    marker: PhantomData<fn() -> Endpoint>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Phase {
    Settings,
    Setup,
    Receive,
    Lifecycle,
    Packets,
}

impl<E: Send + Sync + 'static> NetworkSystems<E> {
    /// Synchronizes this endpoint's limits in `Startup` and `Update`.
    /// Order setting mutations before this set. Queue capacities are captured
    /// by `SETUP`, not updated each frame.
    pub const SETTINGS: Self = Self::new(Phase::Settings);
    /// Creates endpoint resources and starts transport tasks in `Startup`, after
    /// `SETTINGS`. On the client, installs the connection-request observer before
    /// the plugin's optional initial request. Does not wait for a client connection.
    pub const SETUP: Self = Self::new(Phase::Setup);
    /// All incoming events in `PreUpdate`. Use this for protocol-independent
    /// ordering before or after networking. Includes `LIFECYCLE` and `PACKETS`.
    pub const RECEIVE: Self = Self::new(Phase::Receive);
    /// Establishment and closure in `PreUpdate`. For streams, also drains packets
    /// from the same FIFO; do not order a system between this set and `PACKETS`.
    pub const LIFECYCLE: Self = Self::new(Phase::Lifecycle);
    /// Packet publication in `PreUpdate`. For datagrams runs after `LIFECYCLE`
    /// and its deferred commands. For streams selects the same system as `LIFECYCLE`.
    pub const PACKETS: Self = Self::new(Phase::Packets);

    const fn new(phase: Phase) -> Self {
        Self {
            phase,
            marker: PhantomData,
        }
    }
}

// Config and plugin types need not implement Clone, Debug, Eq or Hash.
impl<E: Send + Sync + 'static> Copy for NetworkSystems<E> {}
impl<E: Send + Sync + 'static> Clone for NetworkSystems<E> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<E: Send + Sync + 'static> fmt::Debug for NetworkSystems<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple(std::any::type_name::<Self>())
            .field(&self.phase)
            .finish()
    }
}
impl<E: Send + Sync + 'static> PartialEq for NetworkSystems<E> {
    fn eq(&self, other: &Self) -> bool {
        self.phase == other.phase
    }
}
impl<E: Send + Sync + 'static> Eq for NetworkSystems<E> {}
impl<E: Send + Sync + 'static> Hash for NetworkSystems<E> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.phase.hash(state);
    }
}

/// Exposes networking phases so application systems can order their work around packet events.
///
/// These labels select all configs for a role. Prefer endpoint-specific
/// `ClientSystems<Config>` / `ServerSystems<Config>` to avoid unrelated dependencies.
/// Each plugin processes all incoming events during `PreUpdate`.
/// For stream protocols, packet, establishment and removal labels select one FIFO
/// system with a shared budget in `PreUpdate`. Do not order a system between those
/// labels: they select the same system. Datagram packet processing remains separate.
/// Ordering does not cross schedules.
#[derive(Clone, Debug, Eq, Hash, PartialEq, SystemSet)]
pub enum SystemSets {
    /// All client receive processing in `PreUpdate`, including lifecycle events.
    ClientReceive,
    /// All server receive processing in `PreUpdate`, including lifecycle events.
    ServerReceive,
    /// Publishes packets in `PreUpdate`: with lifecycle for streams, after it for datagrams.
    ClientPacketReceive,
    /// Processes client establishment and closure during `PreUpdate`.
    ClientConnectionEstablish,
    /// The same lifecycle phase as [`Self::ClientConnectionEstablish`].
    ClientConnectionRemove,
    /// Legacy label; built-in connection requests are handled by observers.
    #[deprecated(note = "Connection requests are handled by observers; this set is empty")]
    ClientConnectionRequest,
    /// Legacy label; use [`Self::ServerAcceptNewConnections`] for server lifecycle events.
    #[deprecated(note = "Use ServerAcceptNewConnections; this set is empty")]
    ServerConnectionAdd,
    /// Processes server establishment and closure during `PreUpdate`.
    ServerAcceptNewConnections,
    /// Publishes packets in `PreUpdate`: with lifecycle for streams, after it for datagrams.
    ServerAcceptNewPackets,
    /// The same lifecycle phase as [`Self::ServerAcceptNewConnections`].
    ServerRemoveConnections,
    /// Synchronizes app-local receive limits during `Startup` and `Update`.
    SetMaxPacketSize,
    /// Reports a missing receive-size limit during `Startup`.
    MaxPacketSizeWarning,
}

/// Installs shared receive-limit synchronization once per App.
#[cfg(any(feature = "client", feature = "server"))]
pub fn register_global_limits(app: &mut bevy::prelude::App) {
    use crate::connection::{MaxPacketSize, ReceiveLimits};
    use bevy::prelude::*;

    struct GlobalLimitsPlugin;
    impl Plugin for GlobalLimitsPlugin {
        fn build(&self, app: &mut App) {
            app.init_resource::<ReceiveLimits>()
                .add_systems(
                    Startup,
                    MaxPacketSize::set_system.in_set(SystemSets::SetMaxPacketSize),
                )
                .add_systems(
                    Update,
                    MaxPacketSize::set_system.in_set(SystemSets::SetMaxPacketSize),
                );
        }
    }
    if !app.is_plugin_added::<GlobalLimitsPlugin>() {
        app.add_plugins(GlobalLimitsPlugin);
    }
}
