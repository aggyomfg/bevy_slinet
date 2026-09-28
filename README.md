# bevy_slinet

A simple networking plugin for bevy.

[![docs.rs](https://img.shields.io/docsrs/bevy_slinet)](https://docs.rs/bevy_slinet)
[![Crates.io](https://img.shields.io/crates/v/bevy_slinet)](https://crates.io/crates/bevy_slinet)
[![Crates.io](https://img.shields.io/crates/l/bevy_slinet)](https://github.com/aggyomfg/bevy_slinet/tree/main/LICENSE)

## Features

- You can choose TCP or UDP protocol. Adding your own protocols is as easy as implementing a few traits.
  UDP preserves packet boundaries and supports empty payloads. Delivery is unreliable and unordered; serializers must decode packets independently. A cookie handshake validates return addresses before allocating connections, and session identifiers isolate reconnects. See [UDP configuration](#udp-configuration) for limits and heartbeat settings.
- Multiple clients/servers with different configs (specifies a protocol, packet types, serializer, etc.)
- De/serialization. You choose a serialization format, packet type (you probably want it to be `enum`), and receive events with deserialized packets.

> Note: Everything in bevy_slinet is feature-gated. Make sure to enable features you need (`client`, `server`, `protocol_tcp`, `protocol_udp`, `serializer_bitcode`, `serializer_bitcode_serde`).

> Note: `serializer_bincode` and `serializer_bincode_serde` are kept for compatibility only, since [bincode is unmaintained](https://rustsec.org/advisories/RUSTSEC-2025-0141). Prefer the bitcode serializers for new code.

Note: with TCP, you should implement keep-alive and disconnection systems yourself, or look at [lobby_and_battle_servers example](examples/lobby_and_battle_servers.rs)

## [More Examples](https://github.com/aggyomfg/bevy_slinet/tree/main/examples)

### Compatibility table

| Plugin Version | Bevy Version |
|----------------|--------------|
| `0.9`          | `0.13`       |
| `0.10`         | `0.13`       |
| `0.11`         | `0.14`       |
| `0.12`         | `0.14`       |
| `0.13`         | `0.15`       |
| `0.14`         | `0.16`       |
| `0.15`         | `0.17`       |
| `0.16`         | `0.17`       |
| `0.17`         | `0.18`       |
| `0.18`         | `0.19`       |
| `0.19`         | `0.19`       |
| `main`         | `0.19`       |

## Queue limits

Insert `connection::NetworkQueueSettings` before startup to configure outgoing
packets per connection, incoming packets per plugin, and the event budget per
frame. Defaults are 1024 outgoing packets, 4096 incoming packets/events per
channel, and 256 events per networking system per frame. Capacities count items,
not decoded bytes; serializers must also bound allocations made during decoding.
UDP drops newly received packets when the ECS queue is full. TCP waits for space.
`EcsConnection::send` returns `TrySendError::Full(packet)` on outgoing overflow and
`TrySendError::Closed(packet)` after disconnection. Connection requests also have
a bounded queue; excess requests are rejected with a log message.


## UDP configuration

`UdpProtocol` uses these defaults: 1024 peers per listener, 256 handshake packets
per second, 1200 bytes per outgoing data datagram (including a 37-byte header),
and a one-second heartbeat interval with up to 100 ms of jitter. Recent outgoing
application traffic suppresses heartbeats. Server heartbeats and disconnects
share a response budget. A peer that misses close notifications detects closure
through `UdpIdleTimeout` (10 seconds by default).

To customize settings, implement `protocols::udp::UdpConfig` with an `OPTIONS`
constant and select `ConfiguredUdpProtocol<YourConfig>` as the client/server
protocol. See [the UDP example](examples/hello_world_udp.rs). Allow several
heartbeat intervals, including jitter, when choosing `UdpIdleTimeout`. Timeout
updates wake pending reads and apply to the time of the last valid datagram.

The 1200-byte default leaves 1163 bytes for the serialized payload; it is a
configurable starting point, not path-MTU discovery. Oversized packets and
socket send failures are dropped and logged. The low-level `UdpWriteHalf` exposes
`dropped_oversized_packets()` and `dropped_send_errors()` counters. `MaxPacketSize`
limits received payloads, excluding the session header. The transport ceiling is
65507 bytes including the header; large datagrams may fragment or fail to send.

The SLN2 wire format replaces the previous empty-probe/tag-only format. Upgrade
both endpoints together. The handshake exchanges a padded HELLO, an address-bound
BLAKE3 cookie, a CONFIRM, and an ACCEPT. Challenges are stateless and cookies expire
within 60 seconds. Peer limits also count accepted and superseded streams until
their read side is released. Control replies use nonblocking sends; retries recover
lost handshake replies. All data, heartbeat and disconnect packets carry a session
identifier. Older confirmations cannot replace a newer active session.

Cookies validate reachability; they do not authenticate users or encrypt payloads.
An on-path observer can still see and forge session traffic. The protocol does not
provide application packet retransmission, ordering, or congestion control; applications
must limit their send rate to what the network can sustain.
