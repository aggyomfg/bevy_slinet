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
- [Queue settings and occupancy](../src/connection/queue/mod.rs)
- [Endpoint overrides and fallback rules](../src/connection/settings.rs)
- [UDP options](../src/protocols/udp/settings.rs)
- [System ordering and endpoint phases](../src/scheduling.rs)
- Custom tasks: [RawConnectionParts](../src/connection/parts.rs), extracted with
  `RawConnection::into_parts()` while preserving queued packets. Create a matching
  ECS handle with `RawConnection::with_queue`; use `with_endpoint::<Marker>()`
  on the handle to distinguish your plugin's resource from other endpoints.
- Configured sockets: [TCP](../src/protocols/tcp.rs),
  [UDP client](../src/protocols/udp/stream.rs) and
  [UDP listener](../src/protocols/udp/listener.rs); use their constructors from
  your `Protocol` factories.

`send()` confirms queue acceptance, not delivery; `disconnect()` can discard
pending sends. Local state and queue snapshots do not guarantee the next send
will succeed. Handle send errors and use application acknowledgements when
completion matters.

## System ordering

All incoming events run in `PreUpdate`. Order around
`ClientSystems<Config>::RECEIVE` / `ServerSystems<Config>::RECEIVE` for one endpoint,
or `SystemSets::ClientReceive` / `ServerReceive` for all configs of that role.
For streams, `LIFECYCLE` and `PACKETS` select the same system: do not order between them.

## Runtime

Plugins share one Tokio runtime per App. Set
[`NetworkRuntimeSettings`](../src/runtime/mod.rs) before the first update to configure
it or supply an external multi-thread runtime. An external runtime must outlive the apps using it and enable the drivers required
by its transports (I/O for TCP; I/O and timers for UDP). Shutdown cancels network tasks;
it does not flush queues or forcibly stop blocking user code.

Custom plugins can use `NetworkRuntimePlugin` without `client` / `server` features.
Order task startup after `RuntimeSetup`, then use `NetworkRuntime::spawn` or
`spawn_local`; these tasks participate in the same app shutdown.
