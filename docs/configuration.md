# Configuration

[README](../README.md)

Insert `MaxPacketSize(bytes)` before startup in each Bevy App; the default is
unlimited. This bounds serialized payloads, not allocations made by your decoder.
Low-level connections use `ReceiveLimits`.

Insert `NetworkQueueSettings` before startup to set queue capacities, overflow
policies and per-frame work budgets. Queue capacities count items, not heap bytes.
UDP also has per-peer byte and peer-count limits.

See the definitions for options and defaults:

- [MaxPacketSize, ReceiveLimits and connections](../src/connection/mod.rs)
- [NetworkQueueSettings and OverflowPolicy](../src/connection/queue.rs)
- [UdpConfig and UdpOptions](../src/protocols/udp/settings.rs)
- [SystemSets and scheduling](../src/lib.rs)

`send()` accepts a packet into a queue; it does not confirm delivery.
`disconnect()` can discard pending sends. Use application acknowledgements when
completion matters, and implement retries and peer expiry as needed.
