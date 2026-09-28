# UDP transport

[Back to README](../README.md) · [Queue and receive limits](configuration.md) · [API migration](019-migration.md)

UDP sends exactly the bytes produced by the application's serializer, one packet
per datagram. There is no library header, session ID, length prefix or reserved
packet type. Empty datagrams are supported. Delivery can lose, duplicate or
reorder packets; serializers must decode each datagram independently.

## Packet I/O

`UdpReadHalf` implements `protocol::PacketReader`; `UdpWriteHalf` implements
`protocol::PacketWriter`. Low-level callers use `receive` /
`receive_with_timestamp` and `send`. UDP ignores the length serializer.
Receive calls take [`ReceiveLimits`](configuration.md#receive-limits), checked
before decoding. Malformed or oversized incoming packets are dropped while the
peer continues receiving. A byte-preserving serializer can expose the complete
original datagram to the application.

## Local peers and application sessions

The existing client/server API represents local address associations for UDP:

- Client setup binds a socket and selects the remote address. It sends nothing.
  `ConnectionEstablishEvent` means the local endpoint is ready; it does not
  confirm server availability. Send your first application packet from here.
- The server creates a local peer on the first datagram from an unknown address,
  subject to capacity limits. It publishes `NewConnectionEvent` before packet
  events and preserves the first datagram, including an empty one. This event
  does not authenticate or validate the sender.
- `disconnect()` releases local work and queues. It sends nothing and does not
  notify the other side. UDP has no built-in heartbeat or idle timeout.
- A later datagram from the same address can create another local peer after
  removal. The library does not distinguish delayed packets from a new session.

`ConnectionId` identifies local ECS handles only and is never transmitted.
Applications choose their own session identifiers and packet formats, including
multiple logical sessions per address. Check application session IDs before
using data, updating liveness or processing Close. `PacketReceiveEvent` provides
the decoded packet, its receive timestamp, and the peer's send/close handle.
Use `received_at` rather than event-processing time for liveness decisions.

The [application sessions example](../examples/udp_application_sessions.rs) is a
normal Bevy `App::run()` with `ClientSessions` and `ServerSessions` resources,
packet observers and retry/expiry systems. It uses 48- and 64-byte IDs, exchanges
data, replaces sessions and closes them. Small tests next to the application
session code check ID sizes, independent logical sessions, stale-message rejection
and liveness. Run it with:

```sh
cargo run --example udp_application_sessions --all-features
```

Session identifiers alone do not authenticate a sender. Define authentication,
retransmission, ordering, replay policy and congestion control in the application
when needed. An address change produces a separate local peer; the application
can associate it with an existing session after its own validation.

Local peer allocation precedes application validation. Set queue and peer limits
and explicitly expire or reject pending peers, including peers that only send
malformed payloads. The example retains bounded replay records for its finite
run; long-lived applications must choose a retention policy. A stateless cookie
check before allocating local peer state requires a custom `Protocol`.

## UDP configuration

Implement `protocols::udp::UdpConfig` with an `OPTIONS` constant and select
`ConfiguredUdpProtocol<YourConfig>`. See the
[plain UDP example](../examples/hello_world_udp.rs).

| Setting | Default |
|---------|---------|
| Local peers per listener | 1024 |
| Maximum outgoing serialized datagram | 1200 bytes |
| Raw receive queue per peer | 1024 datagrams / 1 MiB |
| Raw receive overflow policy | `DropNewest` |
| Send-rate limit | Disabled |

All 1200 bytes are available to the application's serialized packet. Its own
session envelope counts towards this budget. `UdpOptions::max_payload_size()`
returns the configured size or `None` if invalid; zero permits only empty
outgoing datagrams. The supported datagram ceiling is 65507 bytes. Large packets
may fragment or fail to send; the limit is not path-MTU discovery and there is
no library fragmentation/reassembly. Oversized outgoing payloads and recoverable
socket send errors are dropped and counted.

`receive_queue_capacity`, `receive_queue_bytes`, and `receive_queue_overflow`
configure each server peer's raw queue. A packet exceeding the byte budget is
rejected without evicting queued packets. The initial datagram must fit too;
otherwise no peer is created. A zero queue capacity admits no peers. Peer limits
also count accepted streams and replaced streams retaining their read halves.
Choose per-peer bounds together with `max_peers` to budget aggregate memory.
Capacity exhaustion produces no automatic response.

## UDP diagnostics and pacing

`EcsConnection::transport()` exposes `&UdpConnectionHandle`:

```rust,ignore
let udp = connection.transport();
let stats = udp.stats();
udp.set_send_rate(std::num::NonZeroU64::new(16 * 1024));
let retained_handle = udp.clone();
```

- `stats()` counts application datagrams and bytes and local queue, codec, size
  and socket drops. Application handshakes count as data too. Byte counts exclude
  OS UDP/IP headers; there is no library overhead.
- `max_payload_size()` exposes the outgoing serialized payload budget.
- `set_send_rate(Option<NonZeroU64>)` changes the serialized-byte rate cap;
  `None` disables pacing. `UdpOptions::send_rate` sets the initial cap.

Pacing spaces sends without accumulating idle burst credit; rate changes and
local cancellation wake a pending send. A byte rate does not limit the number
of empty datagrams. Statistics remain readable through retained handles after
closure. Queue evictions count against the peer whose packet was discarded.
`RawConnection::transport()` and `NetworkStream::transport()` return owned
handles sharing the same state. TCP uses `()`.

Sent counters indicate local socket acceptance, not delivery, loss or RTT.
Datagrams rejected before association with a peer have no per-peer counter.
A rate cap and bounded queues are not adaptive congestion control; applications
must adjust traffic to available capacity, including their own retransmissions.

## Compatibility

Developers must deploy matching application formats on both endpoints. The
library performs no version negotiation. This format is incompatible with the
previous transport framing; see [migration](019-migration.md#udp).
