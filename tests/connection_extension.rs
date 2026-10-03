#![allow(clippy::unwrap_used)]

use std::{
    io,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use bevy::prelude::{App, Plugin};
use bevy_slinet::{
    connection::{
        EcsConnection, NetworkQueueSettings, OverflowPolicy, RawConnection, ReceiveLimits,
        SendError,
    },
    packet_length_serializer::LittleEndian,
    protocol::{
        FramedReader, FramedWriter, NetworkStream, QueueDropReason, ReadStream, TransportHandle,
        WriteStream,
    },
    serializer::Serializer,
};

// Intentionally has no Debug or Default requirement.
#[derive(Clone)]
struct Handle(Arc<Mutex<Vec<QueueDropReason>>>);

impl TransportHandle for Handle {
    fn record_drop(&self, reason: QueueDropReason) {
        self.0.lock().unwrap().push(reason);
    }
}

struct Stream(Handle);

#[cfg_attr(target_family = "wasm", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait::async_trait)]
impl NetworkStream for Stream {
    type Handle = Handle;
    type ReadHalf = FramedReader<UnusedIo>;
    type WriteHalf = FramedWriter<UnusedIo>;

    async fn into_split(self) -> io::Result<(Self::ReadHalf, Self::WriteHalf)> {
        Ok((FramedReader::new(UnusedIo), FramedWriter::new(UnusedIo)))
    }

    fn peer_addr(&self) -> SocketAddr {
        ([127, 0, 0, 1], 2).into()
    }

    fn local_addr(&self) -> SocketAddr {
        ([127, 0, 0, 1], 1).into()
    }

    fn transport(&self) -> Self::Handle {
        self.0.clone()
    }
}

struct UnusedIo;

#[cfg_attr(target_family = "wasm", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait::async_trait)]
impl ReadStream for UnusedIo {
    async fn read_exact(&mut self, _buffer: &mut [u8]) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[cfg_attr(target_family = "wasm", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait::async_trait)]
impl WriteStream for UnusedIo {
    async fn write_all(&mut self, _buffer: &[u8]) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

struct ByteSerializer;

impl Serializer<u8, u8> for ByteSerializer {
    type EncodeError = io::Error;
    type DecodeError = io::Error;

    fn serialize(&self, packet: u8) -> io::Result<Vec<u8>> {
        Ok(vec![packet])
    }

    fn deserialize(&self, data: &[u8]) -> io::Result<u8> {
        match data {
            [packet] => Ok(*packet),
            _ => Err(io::ErrorKind::InvalidData.into()),
        }
    }
}

struct ConnectionEndpoint;

struct ConnectionPlugin(EcsConnection<u8, Handle, ConnectionEndpoint>);

impl Plugin for ConnectionPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(self.0.clone());
    }
}

#[tokio::test]
async fn custom_plugin_shares_queue_limits_and_cancellation_with_its_task() {
    let drops = Arc::new(Mutex::new(Vec::new()));
    let limits = ReceiveLimits::new(32);
    let (raw, connection) = RawConnection::with_queue(
        Stream(Handle(Arc::clone(&drops))),
        Arc::new(ByteSerializer),
        LittleEndian::<u16>::default(),
        NetworkQueueSettings {
            send_capacity: 2,
            ..Default::default()
        },
        limits.clone(),
        false,
    );
    assert_eq!(raw.id(), connection.id());
    assert_eq!(raw.local_addr(), connection.local_addr());
    assert_eq!(raw.peer_addr(), connection.peer_addr());
    let untyped = connection.clone();
    let mut parts = raw.into_parts();
    let mut app = App::new();
    app.add_plugins(ConnectionPlugin(connection.with_endpoint()));
    let retained = app
        .world()
        .resource::<EcsConnection<u8, Handle, ConnectionEndpoint>>()
        .clone();
    assert_eq!(untyped.id(), retained.id());
    retained.send(7).unwrap();
    assert_eq!(parts.packets_rx.recv().await, Some(7));
    assert_eq!(parts.serializer.serialize(8).unwrap(), [8]);
    limits.set_max_packet_size(64);
    assert_eq!(parts.receive_limits.max_packet_size(), 64);

    retained.send(9).unwrap();
    assert_eq!(untyped.outgoing_queue().queued, 1);
    retained.disconnect();
    assert!(parts.disconnect_task.is_cancelled());
    assert!(retained.is_closed());
    assert!(untyped.is_closed());
    assert_eq!(retained.send(10), Err(SendError::Closed(10)));
    drop(parts);
    assert_eq!(
        *drops.lock().unwrap(),
        [QueueDropReason::ClosedBeforeDelivery]
    );
}

#[tokio::test]
async fn custom_queues_apply_stream_and_datagram_policies_without_endpoint_features() {
    for (datagram, overflow) in [
        (false, OverflowPolicy::DropOldest),
        (true, OverflowPolicy::DropNewest),
        (true, OverflowPolicy::DropOldest),
    ] {
        let drops = Arc::new(Mutex::new(Vec::new()));
        let (raw, connection) = RawConnection::with_queue(
            Stream(Handle(Arc::clone(&drops))),
            Arc::new(ByteSerializer),
            LittleEndian::<u16>::default(),
            NetworkQueueSettings {
                send_capacity: 0,
                datagram_send_overflow: overflow,
                ..Default::default()
            },
            ReceiveLimits::default(),
            datagram,
        );
        let mut parts = raw.into_parts();
        assert_eq!(connection.outgoing_queue().capacity, 1);
        connection.send(1).unwrap();
        if datagram && overflow == OverflowPolicy::DropOldest {
            connection.send(2).unwrap();
            assert_eq!(parts.packets_rx.recv().await, Some(2));
            assert_eq!(
                *drops.lock().unwrap(),
                [QueueDropReason::OutgoingQueueEvicted]
            );
        } else {
            assert_eq!(connection.send(2), Err(SendError::Full(2)));
            assert_eq!(parts.packets_rx.recv().await, Some(1));
            assert_eq!(*drops.lock().unwrap(), [QueueDropReason::OutgoingQueueFull]);
        }
        assert_eq!(connection.outgoing_queue().queued, 0);
        parts.disconnect_task.cancel();
        assert!(connection.is_closed());
        assert_eq!(connection.send(3), Err(SendError::Closed(3)));
    }
}
