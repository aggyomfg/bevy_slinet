# Network configuration

[Back to README](../README.md) · [UDP transport](udp.md) · [API migration](migration.md)

## Queue limits

Insert `connection::NetworkQueueSettings` before startup to configure outgoing
packets per connection, incoming packets per plugin, and processing budgets.
Defaults are 1024 outgoing packets, 4096 incoming packets/events per channel,
and 256 events per system per frame. Establishment and closure share one FIFO
lifecycle queue and one frame budget, so closure cannot overtake establishment.
Both lifecycle events are now published in `PreUpdate`. The establishment and
removal system-set labels refer to that shared phase; application systems ordered
around removal must also use `PreUpdate`. Already decoded TCP packets remain
available after EOF; the closed-packet discard policy applies to UDP.

`datagram_send_overflow` and `datagram_receive_overflow` select
`OverflowPolicy::DropNewest` (the default) or `DropOldest`. The latter keeps
fresher queued data by evicting older packets. `EcsConnection::send` means
**queue acceptance**, not delivery: `DropNewest` returns
`connection::SendError::Full(packet)` on overflow; `DropOldest` accepts the new
packet and counts evictions. Closed connections return
`connection::SendError::Closed(packet)`. TCP retains backpressure and does not
evict packets. Connection requests are also bounded; excess requests are
rejected with a log.

Typed queue capacities count items, not decoded heap allocations. Serializers
must bound their own allocations. Raw server UDP queues additionally enforce
exact wire-byte limits. Incoming UDP events wait for connection publication;
packets belonging to closed connections are discarded and counted.

## Receive limits

Insert `connection::MaxPacketSize(bytes)` before startup to limit incoming
serialized payloads. The limit is shared only by plugins in that Bevy App.
Changes apply during `Update`; removing the resource restores the unlimited
default. TCP checks the declared length before allocating its payload; UDP
checks each payload before decoding and discards oversized packets independently.
The UDP session header is excluded from this limit.

Low-level/custom protocols receive an explicit `connection::ReceiveLimits`
argument in `PacketReader::receive` and `receive_with_timestamp`. Custom packet
transports must enforce that limit before allocation or decoding. Byte transports
can use `FramedReader`, which checks the declared length before allocating its
payload. `ReceiveLimits::default()` is unlimited; clones share updates without
affecting other instances.

## Low-level connections and migration

`RawConnection::new` accepts a bounded Tokio MPSC receiver and defaults to
unlimited receive size. Set a limit through
`raw.receive_limits().set_max_packet_size(bytes)`. Raw connection fields are
private; use the constructor and accessors instead of struct literals or field
access.

See the [migration guide](migration.md) for typed transport handles, custom
protocol updates, send errors, and the differences from the older public APIs.

Client `DisconnectionEvent::connection_id` is `Some(id)` for an established
connection and `None` for a failed connection attempt. This distinguishes
connections that share the same server address.
