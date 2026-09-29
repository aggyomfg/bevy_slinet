# Migration to 0.19

[Back to README](../README.md) · [Configuration](configuration.md) · [UDP transport](udp.md)

Changes from `cde760487dc7663c2328a81ec17804e349af4a1f` to 0.19.

## Connections and events

- Replace `EcsConnection<Packet>` with `EcsConnection<Packet, Handle>`, or use
  `client::ClientConnection<Config>` / `server::ServerConnection<Config>`.
  Built-in handles are `()` for TCP and `UdpConnectionHandle` for UDP.
- `send()` now returns `connection::SendError::{Full(packet), Closed(packet)}`
  instead of Tokio's `SendError`. Replace `error.0` with `error.into_inner()`
  and handle queue overflow. Queues are bounded; configure them through
  [`NetworkQueueSettings`](configuration.md#queue-limits).
- `disconnect()` cancels the connection without waiting for its outgoing queue
  to drain. A successful `send()` only enqueues a packet; immediately calling
  `disconnect()` may discard it before it reaches the socket. Keep the connection
  alive until an application acknowledgement arrives when delivery matters.
  With UDP, retry the exchange (including acknowledgements) to handle packet loss;
  an arbitrary delay before disconnecting does not guarantee delivery.
- Move systems ordered around `SystemSets::ClientConnectionRemove` or
  `SystemSets::ServerRemoveConnections` from `PostUpdate` to `PreUpdate`.
- Client `ConnectionEstablishEvent` and `PacketReceiveEvent`, and server
  `NewConnectionEvent`, `DisconnectionEvent`, and `PacketReceiveEvent` are now
  `#[non_exhaustive]`: add `..` to destructuring patterns and stop constructing
  these events with struct literals.
- TCP disconnection events now follow previously queued packet events. Their
  publication may wait for later frames when packet processing is budget-limited.
- Client `DisconnectionEvent` adds `connection_id: Option<ConnectionId>`:
  `Some(id)` for established connections, `None` for failed connection attempts.
- `MaxPacketSize` now applies only within its Bevy App. Set it in each App that
  needs a limit; removing it restores the unlimited default.

## UDP

- Upgrade both endpoints together: each datagram now contains one serialized
  packet without a length prefix. `LengthSerializer` is ignored.
- The client no longer sends empty connection probes. Send an application packet
  after `ConnectionEstablishEvent` to make the server discover the peer. The
  server preserves that first datagram for packet delivery. Client establishment
  means local socket readiness, not confirmation from the server.
- The default maximum serialized payload is now 1200 bytes. Keep packets within
  that budget or configure [UDP limits](udp.md#udp-configuration).
- UDP halves now implement `PacketReader` / `PacketWriter`; replace byte-level
  `read_exact` / `write_all` calls with packet operations. `UdpReadTask` is
  removed; use `PacketReader::receive`.
- `UdpProtocol` is now a type alias; use it as an associated protocol type,
  not as a unit-struct value.

Application handshakes, retries and peer expiry belong in application code; see
the [session example](../examples/udp_application_sessions.rs).

## Custom transports and low-level connections

- Add `type Handle: TransportHandle` to `Protocol` and `NetworkStream`, using
  the same type for the protocol and both streams. Implement
  `NetworkStream::transport(&self) -> Self::Handle`; use `()` if no handle is needed.
- `NetworkStream` halves must implement `PacketReader` / `PacketWriter`.
  For byte transports, wrap existing halves in `FramedReader` / `FramedWriter`.
  Move custom `receive` / `send` implementations from `ReadStream` / `WriteStream`
  to the packet traits. `receive` now takes `&ReceiveLimits`; enforce it before
  allocating or decoding payloads.
- TCP's `into_split()` now returns framed adapters. Update explicit half types
  and import the packet traits for packet I/O; use `get_mut()` / `into_inner()`
  when direct access to the underlying byte stream is needed.
- `RawConnection` fields are private. Use
  `RawConnection::new(stream, serializer, length_serializer, packets_rx)` and
  `id()`, `stream()`, `into_stream()`, or `disconnect()` instead of field access.
  Supply serializers through the constructor. Replace `mpsc::unbounded_channel()`
  with `mpsc::channel(capacity)` for its receiver and use bounded sends.
- Raw connections default to unlimited receive size; set a limit through
  `raw.receive_limits().set_max_packet_size(bytes)`.
- `DisconnectTask` is removed. For standalone cancellation, use
  `tokio_util::sync::CancellationToken` and `cancelled().await`; raw connections
  expose `disconnect()` but no longer expose their cancellation signal.
