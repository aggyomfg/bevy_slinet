# bevy_slinet

A simple networking plugin for bevy.

[![docs.rs](https://img.shields.io/docsrs/bevy_slinet)](https://docs.rs/bevy_slinet)
[![Crates.io](https://img.shields.io/crates/v/bevy_slinet)](https://crates.io/crates/bevy_slinet)
[![Crates.io](https://img.shields.io/crates/l/bevy_slinet)](https://github.com/aggyomfg/bevy_slinet/tree/main/LICENSE)

## Features

- You can choose TCP or UDP protocol. Adding your own protocols is as easy as implementing a few traits.
- Multiple clients/servers with different configs (specifies a protocol, packet types, serializer, etc.)
- De/serialization. You choose a serialization format, packet type (you probably want it to be `enum`), and receive events with deserialized packets.

> Note: Everything in bevy_slinet is feature-gated. Make sure to enable features you need (`client`, `server`, `protocol_tcp`, `protocol_udp`, `serializer_bincode`).

## Client example

```rust
use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use bevy_slinet::client::{ClientPlugin, ConnectionEstablishEvent, PacketReceiveEvent};
use bevy_slinet::packet_length_serializer::LittleEndian;
use bevy_slinet::protocols::tcp::TcpProtocol;
use bevy_slinet::serializers::bincode::{BincodeSerializer, DefaultOptions};
use bevy_slinet::ClientConfig;

struct Config;

impl ClientConfig for Config {
    type ClientPacket = Packet;
    type ServerPacket = Packet;
    type Protocol = TcpProtocol;
    type SerializerError = bincode::Error;
    type LengthSerializer = LittleEndian<u32>;
    fn build_serializer(
    ) -> SerializerAdapter<Self::ClientPacket, Self::ServerPacket, Self::SerializerError> {
        SerializerAdapter::ReadOnly(Arc::new(
            BincodeSerializer::<DefaultOptions>::default(),
        ))
    }
}

#[derive(Serialize, Deserialize, Debug)]
enum ClientPacket {
    String(String),
}

#[derive(Serialize, Deserialize, Debug)]
enum ServerPacket {
    String(String),
}

fn main() {
    App::new()
        .add_plugins(MinimalPlugins)
        .add_plugins(ClientPlugin::<Config>::connect("127.0.0.1:3000"))
        .add_observer(connection_establish_system)
        .add_observer(packet_receive_system)
        .run()
}

fn connection_establish_system(event: Trigger<ConnectionEstablishEvent<Config>>) {
    println!("Connected!");
}

fn packet_receive_system(event: Trigger<PacketReceiveEvent<Config>>) {
    match &event.event().packet {
        ServerPacket::String(s) => println!("Got a message: {}", s),
    }
    event
        .event()
        .connection
        .send(ClientPacket::String("Hello, Server!".to_string()))
        .unwrap();
}
```

## Server Example

```rust
use bevy::prelude::*;
use serde::{Deserialize, Serialize};

use bevy_slinet::packet_length_serializer::LittleEndian;
use bevy_slinet::protocols::tcp::TcpProtocol;
use bevy_slinet::serializers::bincode::{BincodeSerializer, DefaultOptions};
use bevy_slinet::server::{NewConnectionEvent, ServerPlugin, PacketReceiveEvent};
use bevy_slinet::ServerConfig;

struct Config;

impl ServerConfig for Config {
    type ClientPacket = Packet;
    type ServerPacket = Packet;
    type Protocol = TcpProtocol;
    type SerializerError = bincode::Error;
    type LengthSerializer = LittleEndian<u32>;
    fn build_serializer(
    ) -> SerializerAdapter<Self::ClientPacket, Self::ServerPacket, Self::SerializerError> {
        SerializerAdapter::ReadOnly(Arc::new(
            BincodeSerializer::<DefaultOptions>::default(),
        ))
    }
}

#[derive(Serialize, Deserialize, Debug)]
enum ClientPacket {
    String(String),
}

#[derive(Serialize, Deserialize, Debug)]
enum ServerPacket {
    String(String),
}

fn main() {
    App::new()
        .add_plugins(MinimalPlugins)
        .add_plugins(ServerPlugin::<Config>::bind("127.0.0.1:3000").unwrap())
        .add_observer(new_connection_system)
        .add_observer(packet_receive_system)
        .run()
}

fn new_connection_system(event: Trigger<NewConnectionEvent<Config>>) {
    event
        .event()
        .connection
        .send(ServerPacket::String("Hello, Client!".to_string()))
        .unwrap();
}

fn packet_receive_system(event: Trigger<PacketReceiveEvent<Config>>) {
    match &event.event().packet {
        ClientPacket::String(s) => println!("Got a message from a client: {}", s),
    }
    event
        .event()
        .connection
        .send(ServerPacket::String("Hello, Client!".to_string()))
        .unwrap();
}
```

Note: you should implement keep-alive and disconnection systems yourself, or look at [lobby_and_battle_servers example](examples/lobby_and_battle_servers.rs)

## More examples

[Here](https://github.com/aggyomfg/bevy_slinet/tree/main/examples).

### Compatibility table

| Plugin Version | Bevy Version |
|----------------|--------------|
| `0.1`          | `0.6`        |
| `0.2`          | `0.6`        |
| `0.3`          | `0.7`        |
| `0.4`          | `0.8`        |
| `0.5`          | `0.9`        |
| `0.6`          | `0.10.1`     |
| `0.7`          | `0.11`       |
| `0.8`          | `0.12`       |
| `0.9`          | `0.13`       |
| `0.10`         | `0.13`       |
| `0.11`         | `0.14`       |
| `0.12`         | `0.14`       |
| `0.13`         | `0.15`       |
| `main`         | `0.15`       |
