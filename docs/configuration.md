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
- [System ordering](../src/lib.rs)
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
