# bevy_slinet

A simple networking plugin for bevy.

[![docs.rs](https://img.shields.io/docsrs/bevy_slinet)](https://docs.rs/bevy_slinet)
[![Crates.io](https://img.shields.io/crates/v/bevy_slinet)](https://crates.io/crates/bevy_slinet)
[![Crates.io](https://img.shields.io/crates/l/bevy_slinet)](https://github.com/aggyomfg/bevy_slinet/tree/main/LICENSE)

## Features

- You can choose TCP or UDP protocol. Adding your own protocols is as easy as implementing a few traits.
  UDP sends one packet per datagram: delivery is unreliable and unordered. A one-byte data tag allows empty payloads; the serialized payload limit is 65506 bytes, and packets the OS refuses to send are dropped. Serializers must decode packets independently, including after malformed input. Clients connect only after the server answers their probe. UDP peers exchange keep-alives, close connections that stay silent for `UdpIdleTimeout` (10 s by default), and notify each other on disconnect. This wire format is incompatible with older UDP framing; update both peers together. Server keep-alives and disconnect notifications share a response budget; without credit, closure is detected by timeout. Large datagrams may be fragmented or rejected by the OS. The current protocol has no session identifiers or address validation, so delayed packets can affect a new connection reusing the same address. Per-peer UDP queue limits do not bound the downstream ECS queues or total connection count.
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
