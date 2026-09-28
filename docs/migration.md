# Connection API migration

[Back to README](../README.md) · [Queue and receive limits](configuration.md) · [UDP transport](udp.md)

This guide describes migration from the API at
`cde760487dc7663c2328a81ec17804e349af4a1f` to the current API. It covers changes
to APIs available before this branch, without intermediate development history.

## ECS connection types

`EcsConnection<Packet>` becomes `EcsConnection<Packet, Handle>`. The handle type is
required, with no default: a Bevy resource lookup with the wrong handle type
would otherwise silently miss the connection.

Prefer the existing configuration aliases, which select both type parameters:

```rust,ignore
// In an application that defines Config: ClientConfig:
use bevy::prelude::Res;
use bevy_slinet::client::ClientConnection;

fn inspect_connection(connection: Res<ClientConnection<Config>>) {
    println!("Connected to {}", connection.peer_addr());
}
```

Use `server::ServerConnection<Config>` for server connections. When spelling out
`EcsConnection` directly, use `EcsConnection<Packet, ()>` for TCP or
`EcsConnection<Packet, UdpConnectionHandle>` for built-in UDP. Generic helpers
must carry `H: protocol::TransportHandle` as well as the packet bounds.

`connection.transport()` returns `&H`. UDP handles expose `stats()`,
`max_payload_size()`, and `set_send_rate()` directly. Clone the handle when it
must outlive the ECS connection. TCP returns `&()`.

## Send errors and bounded queues

The older `EcsConnection::send` returned Tokio's unbounded-channel
`SendError<Packet>`. It now returns the library's
`connection::SendError<Packet>`:

- `Full(packet)`: the outgoing queue could not accept the packet.
- `Closed(packet)`: the connection is closed.

Replace Tokio error imports and tuple-field access such as `error.0`. Match the
variants to distinguish overload from disconnection, or call `error.into_inner()`
to recover the packet in either case. The error implements `Display` and
`std::error::Error` for packets satisfying the connection's bounds.

```rust,ignore
// In a send call with your application's connection and packet:
use bevy_slinet::connection::SendError;

match connection.send(packet) {
    Ok(()) => {}
    Err(SendError::Full(packet)) => {
        // Keep or discard this packet according to your application's policy.
        drop(packet);
    }
    Err(SendError::Closed(packet)) => {
        // The packet was not accepted; reconnect before retrying it.
        drop(packet);
    }
}
```

Success means queue acceptance, not serialization, socket transmission, or remote
delivery. For datagrams, `DropNewest` rejects a new packet when full;
`DropOldest` accepts it and accounts for the older packet it evicts. TCP uses a
bounded queue without eviction. Configure limits through
[`NetworkQueueSettings`](configuration.md#queue-limits).

## Custom protocols and streams

Add `type Handle: TransportHandle` to each `Protocol` and `NetworkStream`
implementation. A protocol's client and server streams must use the same handle
type as the protocol. Add the required `NetworkStream::transport(&self)` method,
which returns an owned handle. Clones should share connection state so changes
and counters remain visible from the transport and ECS.

For a custom transport without controls or diagnostics, use `()`, which already
implements `TransportHandle`. These are **partial implementation excerpts** to
merge into existing implementations; their listener, I/O, and address methods
are intentionally omitted:

```rust,ignore
#[async_trait::async_trait]
impl bevy_slinet::protocol::Protocol for MyProtocol {
    type Handle = ();
    // Keep Listener, ServerStream, ClientStream, bind, and any overrides.
}

#[async_trait::async_trait]
impl bevy_slinet::protocol::NetworkStream for MyStream {
    type Handle = ();

    fn transport(&self) -> Self::Handle {}

    // Configure packet halves as described below; keep peer_addr and local_addr.
}
```

For diagnostics, define a cloneable handle and implement the optional
`TransportHandle::record_drop(&self, reason: QueueDropReason)` hook. The hook is
called for shared outgoing and decoded receive queue losses; its default does
nothing. Both types are available in `bevy_slinet::protocol` regardless of
whether UDP is enabled. For example, this complete handle definition counts all
reported shared-queue drops:

```rust
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use bevy_slinet::protocol::{QueueDropReason, TransportHandle};

#[derive(Clone, Default)]
struct MyHandle(Arc<AtomicU64>);

impl TransportHandle for MyHandle {
    fn record_drop(&self, _reason: QueueDropReason) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}
```

Set `type Handle = MyHandle` on the protocol and both streams, retain that handle
in each stream, and return `self.handle.clone()` from `transport()`.

### Packet and byte I/O

`ReadStream` and `WriteStream` now describe only byte I/O: `read_exact` and
`write_all`. Packet operations belong to `PacketReader` and `PacketWriter`, and
`NetworkStream::ReadHalf` / `WriteHalf` must implement those packet traits.
Import the new traits from `bevy_slinet::protocol` for packet method calls:

| Previous API | Current API |
|--------------|-------------|
| `ReadStream::receive(serializer, length_serializer)` | `PacketReader::receive(serializer, length_serializer, limits)` |
| `WriteStream::send(packet, serializer, length_serializer)` | `PacketWriter::send(packet, serializer, length_serializer)` |

For a custom byte transport that used the default length-prefix framing, keep
its `ReadStream` / `WriteStream` implementations and wrap its halves in
`FramedReader` / `FramedWriter`. There is no automatic packet implementation for
byte traits. This partial excerpt assumes `self.read` and `self.write` contain
your existing byte halves:

```rust,ignore
use bevy_slinet::protocol::{FramedReader, FramedWriter, NetworkStream};

#[async_trait::async_trait]
impl NetworkStream for MyStream {
    type Handle = ();
    type ReadHalf = FramedReader<MyByteReader>;
    type WriteHalf = FramedWriter<MyByteWriter>;

    async fn into_split(self) -> std::io::Result<(Self::ReadHalf, Self::WriteHalf)> {
        Ok((FramedReader::new(self.read), FramedWriter::new(self.write)))
    }

    fn transport(&self) -> Self::Handle {}
    // Keep peer_addr and local_addr.
}
```

For a custom transport with its own packet boundaries or codec behavior, move
its `receive` override into an implementation of `PacketReader` and its `send`
override into `PacketWriter`. Both methods are required. Use those types
directly as `NetworkStream`'s halves; no `ReadStream`, `WriteStream`,
`read_exact`, or `write_all` implementation is required. The packet methods
retain the packet and serializer type parameters and length-serializer
argument; a datagram transport may ignore the length serializer.

`PacketReader::receive` takes an explicit `&ReceiveLimits` argument. The new
`receive_with_timestamp` method takes it too. Update implementations and
low-level calls to pass it, and enforce the limit before allocating or decoding
payloads. See [receive limits](configuration.md#receive-limits). The default
`receive_with_timestamp` calls your `receive` implementation and timestamps its
completion; override it to capture the transport receive time before decoding.

`PacketReader` also provides optional `close` and `set_idle_timeout` hooks.
`FramedReader` uses their no-op defaults. If a custom byte transport needs
shutdown actions or idle-timeout updates, implement `PacketReader` on your own
wrapper, delegate packet framing to a contained `FramedReader`, and handle
those lifecycle hooks yourself.

TCP's `into_split()` now returns `FramedReader<OwnedReadHalf>` and
`FramedWriter<OwnedWriteHalf>`. Update explicit type annotations and import
`PacketReader` / `PacketWriter` for packet I/O. Both adapters own their byte half:
`new(inner)` wraps it, `get_ref()` / `get_mut()` borrow it, and `into_inner()`
recovers it. For direct TCP byte I/O, use the inner half through `get_mut()` or
`into_inner()`; byte operations bypass framing.

## Raw connections

`RawConnection` remains public, but its fields are private. Replace struct
literals with `RawConnection::new(stream, serializer, length_serializer,
packets_rx)`, and use these accessors:

| Previous field access | Public API |
|-----------------------|------------|
| `raw.id` | `raw.id()` |
| `&raw.stream` | `raw.stream()` |
| Moving out `raw.stream` | `raw.into_stream()` |
| Access to `raw.disconnect_task` | `raw.disconnect()` requests closure; the internal signal is no longer exposed |
| Transport controls through the stream | `raw.transport()` |

`local_addr()` and `peer_addr()` remain available. `raw.transport()` returns
`NS::Handle` sharing the stream's transport state. Receive limits default to
unlimited; update them with
`raw.receive_limits().set_max_packet_size(max_bytes)`.

`into_stream()` consumes the raw connection and drops its outgoing receiver,
discarding any pending outgoing packets. Use it to take ownership of the stream
or split it. There is no mutable stream accessor: replacing a stream would leave
its queue associated with the previous transport handle. Construct a new
`RawConnection` when replacing the stream instead.

The constructor now takes `tokio::sync::mpsc::Receiver<Packet>`. Code migrating
from the comparison commit must replace `mpsc::unbounded_channel()` with
`mpsc::channel(capacity)` and handle bounded sends (`send(...).await` or
`try_send(...)`). The constructor is now available without requiring the `client`
feature.

Serializer, length serializer, and receiver fields no longer have public
replacement APIs. Supply them to the constructor. Retain any serializer or other
shared state you will need later before constructing the raw connection. Use
`into_stream()` when you need to recover the stream itself.

The standalone `DisconnectTask` type has been removed. Use
`tokio_util::sync::CancellationToken` directly and replace `task.await` with
`task.cancelled().await`. A raw
connection no longer exposes its cancellation token for cloning or awaiting.

## Events and system ordering

Connection establishment and removal now share a lifecycle queue processed in
`PreUpdate`. Move systems ordered around `SystemSets::ClientConnectionRemove`
or `SystemSets::ServerRemoveConnections` from `PostUpdate` to `PreUpdate`;
ordering labels do not order systems across schedules.

The following events are now `#[non_exhaustive]`: client
`ConnectionEstablishEvent` and `PacketReceiveEvent`, and server
`NewConnectionEvent`, `DisconnectionEvent`, and `PacketReceiveEvent`. Add `..`
to destructuring patterns. External struct literals are no longer supported;
use your own event types for synthetic application events.

Packet events now expose `received_at`, the transport receive time before
decoding and queueing for built-in protocols. Client
`DisconnectionEvent::connection_id` is `Some(id)` for established connections
and `None` for failed connection attempts.

## UDP compatibility

Upgrade both UDP endpoints together. The previous transport used empty probes
and length-prefixed packet data. The current transport requires the SLN2
handshake and session-framed datagrams; old and new peers cannot communicate.

UDP now ignores `LengthSerializer` and exposes only packet I/O through
`PacketReader::receive` and `PacketWriter::send`. Its halves no longer implement
`ReadStream` / `WriteStream`: replace UDP `read_exact` / `write_all` calls with
packet operations. The public `UdpReadTask` future has been removed; await
`receive` instead. There is no public raw-datagram send API.

Review payload sizes: the default maximum serialized payload is 1163 bytes,
and oversized outgoing packets are dropped. Configure this limit through
[UDP options](udp.md#udp-configuration).

`UdpProtocol` is now a type alias. Its use as a config's associated protocol type
is unchanged; constructing it as a unit-struct value is no longer supported.
