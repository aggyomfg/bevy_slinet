# Configuration

[README](../README.md)

Insert `MaxPacketSize(bytes)` before startup; the default is unlimited. It bounds
serialized payloads, not decoder allocations. `NetworkQueueSettings` controls
queue capacities, overflow policies and per-frame work budgets; capacities count
items, not heap bytes. UDP also has per-peer byte and peer-count limits.

App-wide settings can be overridden per role and config through
`client::ClientSettings<Config>` / `server::ServerSettings<Config>`.
Queue capacities and overflow policies are fixed at startup; frame budgets and
receive-size limits can change at runtime.

Options, defaults and extension points:

- [Connection controls and receive limits](../src/connection/mod.rs)
- [Queue settings and occupancy](../src/connection/queue.rs)
- [Endpoint overrides and fallback rules](../src/connection/settings.rs)
- [UDP options](../src/protocols/udp/settings.rs)
- [System ordering](#system-ordering)
- Custom tasks: [RawConnectionParts](../src/connection/parts.rs), extracted with
  `RawConnection::into_parts()` while preserving queued packets.
- Configured sockets: [TCP](../src/protocols/tcp.rs),
  [UDP client](../src/protocols/udp/stream.rs) and
  [UDP listener](../src/protocols/udp/listener.rs); use their constructors from
  your `Protocol` factories.

`send()` confirms queue acceptance, not delivery; `disconnect()` can discard
pending sends. Local state and queue snapshots do not guarantee the next send
will succeed. Handle send errors and use application acknowledgements when
completion matters.

## System ordering

Plugins install and configure their sets automatically. A set is a label for a
phase, not a queue or a separate scheduler. Use `.before(...)` / `.after(...)`
on your systems, or `app.configure_sets(...)` for your own groups of systems.
Do not add your system to a networking set just to run after networking:
`.in_set(...)` groups systems but does not order them within that set.

Use `client::ClientSystems<Config>` and `server::ServerSystems<Config>` to select
one endpoint. A client and server using the same `Config` still have distinct
sets. `SystemSets` selects all configurations of the corresponding role.

| Set | Schedule | Meaning |
| --- | --- | --- |
| `SETTINGS` | `Startup`, `Update` | Apply this endpoint's receive-size limit |
| `SETUP` | `Startup` | Create resources and start transport tasks, after `SETTINGS` |
| `RECEIVE` | `PreUpdate` | All incoming lifecycle and packet events |
| `LIFECYCLE` | `PreUpdate` | Establishment and closure; also packets for streams |
| `PACKETS` | `PreUpdate` | Packets; after `LIFECYCLE` for datagrams |

For TCP, `LIFECYCLE` and `PACKETS` select **the same FIFO system**, with one shared
budget. Do not chain them or put an application system between them. For UDP,
there are two systems: lifecycle commands are applied before packet processing,
with a separate budget for each system. Both belong to `RECEIVE`.

For example, after adding your plugins (where `GameConfig` is your config type):

```rust
use bevy::prelude::*;
use bevy_slinet::{client::ClientSystems, SystemSets};

// Before limit synchronization; use Startup for initial configuration.
app.add_systems(Update,
    change_client_limits.before(ClientSystems::<GameConfig>::SETTINGS));

// After packet observers have updated application state for this client.
app.add_systems(PreUpdate,
    apply_network_state.after(ClientSystems::<GameConfig>::RECEIVE));

// Or after receive processing for every client config in this App.
app.add_systems(PreUpdate,
    collect_network_metrics.after(SystemSets::ClientReceive));
```

Game systems in `Update` already run after all networking in `PreUpdate`; they
do not need an extra ordering constraint. Ordering labels only work within the
same schedule. With Bevy's default settings, `.after(...)` also applies pending
`Commands`; avoid `after_ignore_deferred` when you need observer effects.
Sets order the systems that trigger events, not individual observers of one event.

For app-wide `MaxPacketSize` changes, order the writer before
`SystemSets::SetMaxPacketSize` so both global and endpoint limits see the change.
Runtime limits synchronize in `Update`, so a change there does not retroactively
change packets already processed in `PreUpdate`.

On startup, client `SETUP` installs the request observer before the plugin's
optional initial connection request. You can trigger requests from your own
system ordered after `SETUP`. This is local initialization, not a guarantee that
a connection is established; wait for the establishment event.

The implementation's schedule graph is declared next to `Plugin::build` in
[client.rs](../src/client.rs) and [server.rs](../src/server.rs), using
`configure_sets` for dependencies and `in_set` for system membership.
Public phase definitions and the once-per-App global limits plugin live in
[scheduling.rs](../src/scheduling.rs).
