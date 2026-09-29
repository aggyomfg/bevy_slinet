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

## Configured sockets

The built-in transports can wrap sockets created by your own `Protocol` factories:

- TCP: `TcpNetworkStream::from_stream(tokio_stream)` preserves socket options.
  `socket()` lets you inspect or set options before splitting. Wrap an existing
  listener with `TcpNetworkListener::from_listener(tokio_listener)` and use
  `.with_nodelay(true)` to apply `TCP_NODELAY` to every accepted connection.
- UDP: `UdpClientStream::from_socket(connected_socket, options)` preserves a chosen
  source address/port. Bind, configure and connect the Tokio socket first.
  `UdpNetworkListener::from_socket(bound_socket, options)` requires an unconnected
  socket so it can receive from multiple peers. Both constructors validate options.
  Runtime options are also accepted by `UdpNetworkListener::bind` and
  `UdpClientStream::connect_with_options`.

Use these constructors inside `Protocol::bind` and `Protocol::connect_to_server`,
then select that protocol in your client/server config. A custom UDP protocol must
set `const DATAGRAM: bool = true` so the plugin uses datagram queue and event
semantics. The [TCP module](../src/protocols/tcp.rs)
contains a complete factory example. Existing packet framing and UDP queues,
limits, pacing and diagnostics are reused. Creating a wrapper alone does not
attach it to an already-running plugin; the plugin calls your protocol factories
on its own Tokio runtime.

For OS-specific keepalive, buffer sizes or bind options, configure the socket
before passing it to the constructor. Standard-library sockets must be put into
nonblocking mode before converting them to Tokio sockets. Server-side per-connection
options beyond `TCP_NODELAY` can be applied by a custom `Listener` before wrapping
its accepted stream.
