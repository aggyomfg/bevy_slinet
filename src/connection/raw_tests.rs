use std::fmt::Debug;
use std::io;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

#[cfg(any(feature = "client", feature = "server"))]
use super::{NetworkQueueSettings, SendError};
use super::{RawConnection, ReceiveLimits};
use crate::protocols::protocol::{
    NetworkStream, PacketReader, PacketWriter, QueueDropReason, ReceiveError, TransportHandle,
};
use crate::serializers::packet_length_serializer::{LittleEndian, PacketLengthSerializer};
use crate::serializers::serializer::Serializer;

// Custom handles need neither Debug nor Default, including through constructors.
#[derive(Clone)]
struct SharedHandle(Arc<Mutex<Vec<QueueDropReason>>>);

impl TransportHandle for SharedHandle {
    fn record_drop(&self, reason: QueueDropReason) {
        self.0.lock().unwrap().push(reason);
    }
}

struct TestStream {
    handle: SharedHandle,
    local_addr: SocketAddr,
    peer_addr: SocketAddr,
}

impl TestStream {
    fn new(handle: SharedHandle) -> Self {
        Self {
            handle,
            local_addr: SocketAddr::from(([127, 0, 0, 1], 1234)),
            peer_addr: SocketAddr::from(([127, 0, 0, 1], 5678)),
        }
    }
}

#[async_trait::async_trait]
impl NetworkStream for TestStream {
    type Handle = SharedHandle;
    type ReadHalf = UnsupportedIo;
    type WriteHalf = UnsupportedIo;

    async fn into_split(self) -> io::Result<(Self::ReadHalf, Self::WriteHalf)> {
        Err(io::ErrorKind::Unsupported.into())
    }

    fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    fn transport(&self) -> Self::Handle {
        self.handle.clone()
    }
}

struct UnsupportedIo;

#[async_trait::async_trait]
impl PacketReader for UnsupportedIo {
    async fn receive<ReceivingPacket, SendingPacket, S, LS>(
        &mut self,
        _serializer: Arc<S>,
        _length_serializer: &LS,
        _limits: &ReceiveLimits,
    ) -> Result<ReceivingPacket, ReceiveError<S::DecodeError, LS::Error>>
    where
        ReceivingPacket: Send + Sync + Debug + 'static,
        SendingPacket: Send + Sync + Debug + 'static,
        S: Serializer<ReceivingPacket, SendingPacket> + ?Sized,
        LS: PacketLengthSerializer,
    {
        Err(ReceiveError::Io(io::ErrorKind::Unsupported.into()))
    }
}

#[async_trait::async_trait]
impl PacketWriter for UnsupportedIo {
    async fn send<ReceivingPacket, SendingPacket, S, LS>(
        &mut self,
        _packet: SendingPacket,
        _serializer: Arc<S>,
        _length_serializer: &LS,
    ) -> io::Result<()>
    where
        ReceivingPacket: Send + Sync + Debug + 'static,
        SendingPacket: Send + Sync + Debug + 'static,
        S: Serializer<ReceivingPacket, SendingPacket> + ?Sized,
        LS: PacketLengthSerializer,
    {
        Err(io::ErrorKind::Unsupported.into())
    }
}

struct ByteSerializer;

impl Serializer<u8, u8> for ByteSerializer {
    type EncodeError = io::Error;
    type DecodeError = io::Error;

    fn serialize(&self, packet: u8) -> Result<Vec<u8>, Self::EncodeError> {
        Ok(vec![packet])
    }

    fn deserialize(&self, data: &[u8]) -> Result<u8, Self::DecodeError> {
        match data {
            [packet] => Ok(*packet),
            _ => Err(io::ErrorKind::InvalidData.into()),
        }
    }
}

#[cfg(any(feature = "client", feature = "server"))]
#[test]
fn constructor_attaches_handle_before_an_unpublished_connection_is_dropped() {
    let drops = Arc::new(Mutex::new(Vec::new()));
    let handle = SharedHandle(Arc::clone(&drops));
    let (sender, receiver) = NetworkQueueSettings::default().outgoing_channel(true);
    let raw = RawConnection::with_limits(
        TestStream::new(handle),
        Arc::new(ByteSerializer),
        LittleEndian::<u32>::default(),
        receiver,
        ReceiveLimits::new(128),
    );
    let connection = raw.ecs_connection::<()>(sender);
    assert_eq!(connection.send(7), Ok(()));
    assert!(drops.lock().unwrap().is_empty());

    drop(raw);

    assert_eq!(
        *drops.lock().unwrap(),
        [QueueDropReason::ClosedBeforeDelivery]
    );
    assert_eq!(connection.send(8), Err(SendError::Closed(8)));
    assert_eq!(
        *drops.lock().unwrap(),
        [QueueDropReason::ClosedBeforeDelivery]
    );
}

#[test]
fn recovering_stream_closes_the_owned_receiver_and_preserves_shared_handle() {
    let drops = Arc::new(Mutex::new(Vec::new()));
    let handle = SharedHandle(Arc::clone(&drops));
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    let raw = RawConnection::new(
        TestStream::new(handle),
        Arc::new(ByteSerializer),
        LittleEndian::<u32>::default(),
        receiver,
    );
    sender.try_send(7).unwrap();
    assert!(!sender.is_closed());

    let stream = raw.into_stream();

    assert!(sender.is_closed());
    assert!(matches!(
        sender.try_send(8),
        Err(tokio::sync::mpsc::error::TrySendError::Closed(8))
    ));
    assert_eq!(
        *drops.lock().unwrap(),
        [QueueDropReason::ClosedBeforeDelivery]
    );
    stream
        .transport()
        .record_drop(QueueDropReason::ReceiveQueueFull);
    assert_eq!(
        *drops.lock().unwrap(),
        [
            QueueDropReason::ClosedBeforeDelivery,
            QueueDropReason::ReceiveQueueFull
        ]
    );
}

#[cfg(any(feature = "client", feature = "server"))]
#[test]
fn raw_disconnect_closes_ecs_clones_without_counting_rejected_packets() {
    let drops = Arc::new(Mutex::new(Vec::new()));
    let handle = SharedHandle(Arc::clone(&drops));
    let (sender, receiver) = NetworkQueueSettings::default().outgoing_channel(true);
    let raw = RawConnection::with_limits(
        TestStream::new(handle),
        Arc::new(ByteSerializer),
        LittleEndian::<u32>::default(),
        receiver,
        ReceiveLimits::default(),
    );
    let connection = raw.ecs_connection::<()>(sender);
    let retained = connection.clone();
    assert_eq!(connection.id(), raw.id());
    assert_eq!(retained.id(), raw.id());

    raw.disconnect();

    assert_eq!(connection.send(7), Err(SendError::Closed(7)));
    assert_eq!(retained.send(8), Err(SendError::Closed(8)));
    drop(raw);
    assert!(drops.lock().unwrap().is_empty());
}

#[tokio::test]
async fn extracting_parts_keeps_queue_codecs_and_shared_controls_alive() {
    let drops = Arc::new(Mutex::new(Vec::new()));
    let handle = SharedHandle(Arc::clone(&drops));
    let (sender, receiver) = tokio::sync::mpsc::channel(2);
    let raw = RawConnection::new(
        TestStream::new(handle),
        Arc::new(ByteSerializer),
        LittleEndian::<u32>::default(),
        receiver,
    );
    let id = raw.id();
    let limits = raw.receive_limits().clone();
    let cancellation = raw.disconnect_task.clone();
    sender.try_send(7).unwrap();
    let mut parts = raw.into_parts();
    assert_eq!(parts.id, id);
    assert!(!sender.is_closed());
    assert_eq!(parts.packets_rx.recv().await, Some(7));
    assert_eq!(parts.serializer.serialize(8).unwrap(), vec![8]);
    limits.set_max_packet_size(42);
    assert_eq!(parts.receive_limits.max_packet_size(), 42);
    parts.disconnect_task.cancel();
    assert!(cancellation.is_cancelled());
    sender.try_send(9).unwrap();
    drop(parts);
    assert!(sender.is_closed());
    assert_eq!(
        *drops.lock().unwrap(),
        [QueueDropReason::ClosedBeforeDelivery]
    );
}

#[cfg(any(feature = "client", feature = "server"))]
#[tokio::test]
async fn ecs_queue_snapshot_tracks_drain_eviction_and_close() {
    use super::{OverflowPolicy, QueueSnapshot};
    for (datagram, policy) in [
        (false, OverflowPolicy::DropNewest),
        (true, OverflowPolicy::DropNewest),
        (true, OverflowPolicy::DropOldest),
    ] {
        let settings = NetworkQueueSettings {
            send_capacity: 2,
            datagram_send_overflow: policy,
            ..Default::default()
        };
        let (tx, rx) = settings.outgoing_channel(datagram);
        let raw = RawConnection::with_limits(
            TestStream::new(SharedHandle(Arc::default())),
            Arc::new(ByteSerializer),
            LittleEndian::<u32>::default(),
            rx,
            ReceiveLimits::default(),
        );
        let connection = raw.ecs_connection::<()>(tx);
        let retained = connection.clone();
        let mut parts = raw.into_parts();
        assert!(!connection.is_closed());
        assert_eq!(
            connection.outgoing_queue(),
            QueueSnapshot {
                queued: 0,
                capacity: 2
            }
        );
        connection.send(1).unwrap();
        retained.send(2).unwrap();
        if datagram && policy == OverflowPolicy::DropOldest {
            connection.send(3).unwrap();
        } else {
            assert_eq!(connection.send(3), Err(SendError::Full(3)));
        }
        assert_eq!(
            retained.outgoing_queue(),
            QueueSnapshot {
                queued: 2,
                capacity: 2
            }
        );
        assert_eq!(
            parts.packets_rx.recv().await,
            Some(if datagram && policy == OverflowPolicy::DropOldest {
                2
            } else {
                1
            })
        );
        assert_eq!(connection.outgoing_queue().queued, 1);
        drop(parts);
        assert!(connection.is_closed());
        assert!(retained.is_closed());
        assert_eq!(retained.send(4), Err(SendError::Closed(4)));
    }
}

#[cfg(any(feature = "client", feature = "server"))]
#[test]
fn ecs_connection_component_removal_preserves_retained_handle() {
    use super::EcsConnection;
    let (tx, rx) = NetworkQueueSettings::default().outgoing_channel(false);
    let raw = RawConnection::with_limits(
        TestStream::new(SharedHandle(Arc::default())),
        Arc::new(ByteSerializer),
        LittleEndian::<u32>::default(),
        rx,
        ReceiveLimits::default(),
    );
    let retained = raw.ecs_connection::<()>(tx);
    let mut world = bevy::prelude::World::new();
    let entity = world.spawn(retained.clone()).id();
    let mut query = world.query::<&EcsConnection<u8, SharedHandle>>();
    let connection = query.single(&world).unwrap();
    assert_eq!(connection.id(), retained.id());
    connection.send(7).unwrap();
    world
        .entity_mut(entity)
        .remove::<EcsConnection<u8, SharedHandle>>();
    assert!(!retained.is_closed());
    retained.send(8).unwrap();
    retained.disconnect();
    assert!(retained.is_closed());
    assert_eq!(retained.send(9), Err(SendError::Closed(9)));
    // Receiver still exists: cancellation alone must be enough to report closure.
    assert_eq!(retained.outgoing_queue().queued, 2);
    drop(raw);
}
