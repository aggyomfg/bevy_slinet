# `bevy_slinet`

TCP/UDP networking for Bevy with configurable packet types, serializers and custom transports.

[![docs.rs](https://img.shields.io/docsrs/bevy_slinet)](https://docs.rs/bevy_slinet)
[![Crates.io](https://img.shields.io/crates/v/bevy_slinet)](https://crates.io/crates/bevy_slinet)
[![Crates.io](https://img.shields.io/crates/l/bevy_slinet)](https://github.com/aggyomfg/bevy_slinet/tree/main/LICENSE)

Enable the features you need: `client`, `server`, `protocol_tcp`, `protocol_udp`,
`serializer_bitcode` or `serializer_bitcode_serde`. See [Cargo.toml](https://github.com/aggyomfg/bevy_slinet/blob/main/Cargo.toml).
Bincode serializers remain for compatibility; prefer bitcode for new code.

Browser WASM support is preliminary and requires a custom transport; the built-in
TCP/UDP transports are native-only. See the [configuration guide](https://github.com/aggyomfg/bevy_slinet/blob/main/docs/configuration.md#browser-wasm-preliminary).

- Examples: [TCP](https://github.com/aggyomfg/bevy_slinet/blob/main/examples/hello_world_tcp.rs), [UDP](https://github.com/aggyomfg/bevy_slinet/blob/main/examples/hello_world_udp.rs),
  [application sessions](https://github.com/aggyomfg/bevy_slinet/blob/main/examples/udp_application_sessions.rs), [all examples](https://github.com/aggyomfg/bevy_slinet/tree/main/examples).
- [API reference](https://docs.rs/bevy_slinet) · [Configuration](https://github.com/aggyomfg/bevy_slinet/blob/main/docs/configuration.md) ·
  [UDP semantics](https://github.com/aggyomfg/bevy_slinet/blob/main/docs/udp.md) · [Migration to 0.19](https://github.com/aggyomfg/bevy_slinet/blob/main/docs/019-migration.md) ·
  [Benchmarks](https://github.com/aggyomfg/bevy_slinet/blob/main/docs/benchmarks.md).

## Bevy compatibility

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
