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

## Per-endpoint overrides

Insert `client::ClientSettings::<MyConfig>::default()` or
`server::ServerSettings::<MyConfig>::default()` with `.with_queues(settings)` and/or
`.with_max_packet_size(bytes)` before startup. These resources are independent for
both roles and every config type, even when they share one App. Their public
`queues` and `max_packet_size` fields are optional: `None` inherits the corresponding
app-wide resource. Removing an override restores that fallback.

Queue capacities and overflow policies are captured at startup. Changing them
later does not resize existing queues. `events_per_frame` is read on every network
ECS phase (zero pauses that endpoint's event processing). Packet size limits are
synchronized at startup and `Update`, including for existing connections;
`Some(usize::MAX)` explicitly disables an endpoint's size bound. An already-running
packet read may finish using its previous limit.

## Custom connection tasks

`RawConnection::into_parts()` transfers the stream, outgoing receiver, payload and
length serializers, cancellation token, live receive limits and connection ID.
Unlike `into_stream()`, it preserves queued packets. A custom task must observe
`disconnect_task` and use `receive_limits` when calling `PacketReader::receive`.
Dropping the receiver closes the queue and records undelivered queued packets;
extracting parts does not itself cancel the connection.
