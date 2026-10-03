# Migration to 0.19

[README](../README.md)

## Connections and events

- Use `ClientConnection<Config>` / `ServerConnection<Config>`, or replace
  `EcsConnection<Packet>` with `EcsConnection<Packet, Handle>` (`()` for TCP,
  `UdpConnectionHandle` for UDP). The endpoint aliases also carry a third type
  parameter for their role and config; prefer these aliases in application APIs.
- `send()` returns `connection::SendError::{Full(packet), Closed(packet)}`.
  Replace Tokio error imports and `error.0` with `error.into_inner()`; handle
  bounded-queue overflow. See [configuration](configuration.md).
- Move systems ordered around `ClientConnectionRemove` / `ServerRemoveConnections`
  to `PreUpdate`. Move systems around `ClientPacketReceive` there too, for both
  TCP and UDP. Stream packet and lifecycle labels now select one FIFO-draining
  system with a shared frame budget; order before or after the whole phase.
  TCP disconnection follows queued packet events, potentially in a later frame.
- Client `ConnectionEstablishEvent` / `PacketReceiveEvent` and server
  `NewConnectionEvent` / `DisconnectionEvent` / `PacketReceiveEvent` are
  `#[non_exhaustive]`: add `..` to patterns; external struct literals are unsupported.
- Client `DisconnectionEvent` adds `connection_id: Option<ConnectionId>`
  (`None` for failed attempts). Rejected connection requests now emit this event too.
- `MaxPacketSize` is now App-local; set it in each App that needs a limit.

## Scheduling labels

- Prefer `client::ClientSystems<Config>` / `server::ServerSystems<Config>` for
  ordering one endpoint. `RECEIVE` covers all incoming events in `PreUpdate`;
  `SETTINGS` covers limit synchronization and `SETUP` covers startup initialization.
  See [system ordering](configuration.md#system-ordering) for the complete graph.
- `SystemSets::ClientReceive` / `ServerReceive` cover all configs of that role.
  Existing active labels remain supported. `ClientConnectionRequest` and
  `ServerConnectionAdd` are deprecated empty labels; connection requests use observers.
- UDP client packets now publish in `PreUpdate`, before gameplay in `Update`.
  Packets arriving later in the frame wait for the next `PreUpdate`.
  Replace `PostUpdate` ordering around `ClientPacketReceive` with `PreUpdate`.

## UDP

- Upgrade both endpoints: datagrams now contain serialized packets without a
  length prefix. `LengthSerializer` is ignored; the default payload limit is 1200 bytes.
- Empty connection probes are gone. Send an application packet after client
  establishment so the server discovers the peer; the first datagram is preserved.
  See [UDP semantics](udp.md) and the [session example](../examples/udp_application_sessions.rs).
- Replace byte I/O and `UdpReadTask` with `PacketReader` / `PacketWriter` operations.
- `UdpProtocol` is now a type alias; use it as a type, not a unit-struct value.

## Custom transports and raw connections

- Add matching `type Handle: TransportHandle` to `Protocol` and both
  `NetworkStream` implementations, plus `transport(&self) -> Self::Handle`.
  Use `()` when no handle is needed.
- Stream halves now implement `PacketReader` / `PacketWriter`. Wrap byte halves
  in `FramedReader` / `FramedWriter`, or move custom packet methods to the new
  traits. `receive` takes `&ReceiveLimits`; enforce it before allocation/decoding.
  TCP halves are now framed adapters too: update explicit types and trait imports.
- `RawConnection` fields are private: use its constructor and accessors, or
  `into_parts()` to transfer state to a custom task. Replace its unbounded MPSC
  channel with `mpsc::channel(capacity)` and bounded sends. Custom tasks must
  observe the cancellation token and pass receive limits to packet readers.
- `DisconnectTask` is replaced by `CancellationToken`; replace `task.await`
  with `task.cancelled().await`.
