# UDP transport

[Back to README](../README.md) · [Queue and receive limits](configuration.md) · [API migration](migration.md)

UDP preserves packet boundaries and supports empty payloads. Delivery is unreliable and unordered; serializers must decode packets independently. A cookie handshake validates return addresses before allocating connections, and session identifiers isolate reconnects.

## Packet I/O

`UdpReadHalf` implements `protocol::PacketReader`; `UdpWriteHalf` implements
`protocol::PacketWriter`. Low-level callers use `receive` /
`receive_with_timestamp` and `send` with their packet serializer. UDP ignores
the length-serializer argument because each DATA datagram carries one packet.
Receive calls also take [`ReceiveLimits`](configuration.md#receive-limits).
The halves provide no byte-stream operations or public raw-datagram send API.

## UDP configuration

Implement `protocols::udp::UdpConfig` with an `OPTIONS` constant and select
`ConfiguredUdpProtocol<YourConfig>` as the client/server protocol. See
[the UDP example](../examples/hello_world_udp.rs).

| Setting | Default |
|---------|---------|
| Registered peers per listener | 1024 |
| HELLO/CONFIRM packets per second per listener | 256 |
| Retained replay-protection addresses | 16384 |
| Outgoing DATA datagram, including the 37-byte session header | 1200 bytes |
| Raw receive queue per peer | 1024 datagrams / 1 MiB |
| Raw receive overflow policy | `DropNewest` |
| Total connection deadline | 5 seconds |
| Initial / maximum handshake retry interval | 1 / 4 seconds |
| Additional retry jitter | 0–100 ms |
| Heartbeat interval / additional jitter | 1 second / 0–100 ms |
| App-local `UdpIdleTimeout` | 10 seconds |
| DATA send-rate limit | Disabled |

Handshake requests start immediately. Unanswered requests use exponential
backoff; the first challenge starts the confirmation leg without resetting the
overall deadline. The client keeps the selected challenge throughout the attempt.
Malformed or duplicate traffic cannot extend that deadline.

Recent outgoing application traffic suppresses heartbeats. Server heartbeats
and disconnects share a response budget. Up to three close notifications are
sent at one-second intervals; losing them all is recovered through idle timeout.
Allow several heartbeat intervals, including jitter, when setting `UdpIdleTimeout`.
Updates wake pending reads and apply to the last valid session datagram;
`Duration::MAX` disables idle expiry. This heartbeat detects liveness, rather than
merely keeping a NAT mapping open.

The 1200-byte default leaves 1163 bytes for serialized payloads.
`UdpOptions::max_payload_size()` returns the configured usable size, or `None` for
an invalid datagram size. This is a configurable starting point, not path-MTU
discovery. The absolute datagram ceiling is 65507 bytes including the header;
large datagrams may fragment in IP or fail to send. There is no application
fragmentation/reassembly. Oversized payloads and socket send failures are dropped
and counted without closing a healthy session.

`receive_queue_capacity`, `receive_queue_bytes`, and `receive_queue_overflow`
configure each server peer's raw queue. An individually oversized datagram is
rejected without evicting queued packets. Limits are per peer: choose them
together with `max_peers` to budget aggregate memory.

## UDP diagnostics and pacing

For a UDP connection, `EcsConnection::transport()` returns
`&UdpConnectionHandle`. Its type is selected by the configured protocol, so UDP
controls are available directly, without an `Option` check:

```rust,ignore
// `connection` is a ClientConnection<Config> or ServerConnection<Config>
// whose Config::Protocol is UdpProtocol or ConfiguredUdpProtocol<_>.
let udp = connection.transport();
let stats = udp.stats();
udp.set_send_rate(std::num::NonZeroU64::new(16 * 1024));
let retained_handle = udp.clone();
```

The handle provides:

- `stats()`: cumulative data/control packet and byte counters, plus local drop
  reasons for outgoing, raw receive and ECS queues, malformed payloads, size
  limits and socket errors. Bytes include the SLN2 header.
- `max_payload_size()`: the configured serialized payload budget.
- `set_send_rate(Option<NonZeroU64>)`: change the DATA rate cap in wire bytes per
  second; `None` disables pacing. `UdpOptions::send_rate` sets the initial cap.

The writer serializes once, then spaces DATA sends without accumulating a burst
allowance while idle. A changed rate or disconnection wakes a pending pacing wait.
Control traffic uses its own handshake timers and response budget. Statistics
remain readable after disconnection while a handle is retained. Shared ECS queue
evictions are charged to the connection whose packet was discarded.
`RawConnection::transport()` and `NetworkStream::transport()` return an owned
handle sharing the same state. TCP uses `()` as its transport handle.

Sent counters mean **accepted by the local socket**. They are not remote delivery
acknowledgments or measurements of network loss or RTT. Traffic that cannot be
associated with a session is not included in a connection's counters. There is
no event per drop. Applications can take snapshot differences to derive local
rates and supply their own delivery feedback.

A fixed rate cap and bounded queues are not adaptive congestion control.
Applications must adjust traffic to the network's available capacity, including
any retransmissions they implement. See [RFC 8085](https://www.rfc-editor.org/rfc/rfc8085.html#section-3.1).

## UDP sessions and compatibility

SLN2 exchanges a padded HELLO, an address-bound BLAKE3 cookie, CONFIRM and ACCEPT.
Challenges are stateless; cookies expire within 60 seconds. A bounded table keeps
the highest admitted generation per address until its cookie expires, even after
the connection closes. It never evicts live protection records to admit another
address: new admissions can temporarily fail when the table is full. Older
confirmations cannot resurrect a completed session or replace a newer one.
Failed replacements preserve the current connection. Peer limits also count
accepted and superseded streams until their read side is released.

Data, heartbeat and disconnect packets carry a session identifier. Both
endpoints must use SLN2; the previous empty-probe, length-prefixed UDP transport
is incompatible. Upgrade both endpoints together; see the
[migration guide](migration.md#udp-compatibility). Cookies validate reachability,
not user identity; payloads are not encrypted and an on-path observer can forge traffic.
The transport provides no application retransmission, delivery ordering,
authentication, automatic congestion control, or address migration.
