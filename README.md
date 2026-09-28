# `bevy_slinet`

A simple networking plugin for bevy.

[![docs.rs](https://img.shields.io/docsrs/bevy_slinet)](https://docs.rs/bevy_slinet)
[![Crates.io](https://img.shields.io/crates/v/bevy_slinet)](https://crates.io/crates/bevy_slinet)
[![Crates.io](https://img.shields.io/crates/l/bevy_slinet)](https://github.com/aggyomfg/bevy_slinet/tree/main/LICENSE)

## Features

- You can choose TCP or UDP protocol. Adding your own protocols is as easy as implementing a few traits.
- Multiple clients/servers with different configs (specifies a protocol, packet types, serializer, etc.)
- De/serialization. You choose a serialization format, packet type (you probably want it to be `enum`), and receive events with deserialized packets.

> Note: Everything in bevy_slinet is feature-gated. Make sure to enable features you need (`client`, `server`, `protocol_tcp`, `protocol_udp`, `serializer_bitcode`, `serializer_bitcode_serde`).

> Note: `serializer_bincode` and `serializer_bincode_serde` are kept for compatibility only, since [bincode is unmaintained](https://rustsec.org/advisories/RUSTSEC-2025-0141). Prefer the bitcode serializers for new code.

Note: with TCP, you should implement keep-alive and disconnection systems yourself, or look at [`lobby_and_battle_servers` example](examples/lobby_and_battle_servers.rs)

## Documentation

- [Queue and receive limits](docs/configuration.md)
- [API migration: typed transport handles and connection APIs](docs/019-migration.md)
- [UDP: configuration, diagnostics, pacing, and application sessions](docs/udp.md)
- [API reference](https://docs.rs/bevy_slinet)

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
