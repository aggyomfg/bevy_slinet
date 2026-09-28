//! Implement [`Protocol`] to create your own protocol implementation and use
//! it in [`ServerConfig`](crate::ServerConfig) or [`ClientConfig`](crate::ClientConfig).
//!
//! Built-in protocols are listed in the [`protocols`](crate::protocols) module.

use bevy::log;
use bevy::platform::time::Instant;
use std::error::Error;
use std::fmt::{Debug, Formatter};
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use async_trait::async_trait;

use crate::connection::MAX_PACKET_SIZE;
use crate::serializers::packet_length_serializer::PacketLengthDeserializationError;
use crate::serializers::serializer::Serializer;
use crate::PacketLengthSerializer;

/// In order to simplify protocol switching and implementation, there is a [`Protocol`] trait.
/// Implement it or use built-in [`protocols`](crate::protocols).
#[async_trait]
pub trait Protocol: Send + Sync + 'static {
    /// A server-side listener type.
    type Listener: Listener<Stream = Self::ServerStream>;
    /// A server-side network stream. It can be different from [`Self::ClientStream`]
    type ServerStream: ServerStream;
    /// A client-side network stream. It can be different from [`Self::ServerStream`]
    type ClientStream: ClientStream;

    /// `true` if packets may be lost, duplicated or reordered, as with UDP.
    const DATAGRAM: bool = false;

    /// Creates a [Listener](Self::Listener).
    async fn bind(addr: SocketAddr) -> io::Result<Self::Listener>;

    /// Connect to the server at specified address.
    async fn connect_to_server(addr: SocketAddr) -> io::Result<Self::ClientStream> {
        let stream = Self::ClientStream::connect(addr).await?;
        log::debug!("Connected to a server at {:?}", stream.peer_addr());
        Ok(stream)
    }
}

/// A listener that accepts connections from clients.
#[async_trait]
pub trait Listener {
    /// A [`ServerStream`] that is returned by [`Self::accept()`]
    type Stream: ServerStream;

    /// Returns a [ServerStream](ServerStream) when a client wants to connect.
    async fn accept(&self) -> io::Result<Self::Stream>;

    /// Returns the bound endpoint, including the assigned port when bound to port zero.
    fn address(&self) -> SocketAddr;

    /// Releases listener-side state after a connection’s receive task closes.
    fn handle_disconnection(&self, _peer_addr: SocketAddr) {}
}

/// A [NetworkStream](NetworkStream) that can be used client-side.
#[async_trait]
pub trait ClientStream: NetworkStream {
    /// Connects to a server.
    async fn connect(addr: SocketAddr) -> io::Result<Self>
    where
        Self: Sized;
}

/// A [`NetworkStream`] that can be used server-side.
pub trait ServerStream: NetworkStream {}

/// A read-write stream between the client and the server.
#[async_trait]
pub trait NetworkStream: Send + Sync + 'static {
    /// A read half of this stream.
    type ReadHalf: ReadStream;
    /// A write half of this stream.
    type WriteHalf: WriteStream;

    /// Splits this stream into read and write half to use them in different futures.
    async fn into_split(self) -> io::Result<(Self::ReadHalf, Self::WriteHalf)>;

    /// Returns the socket address of the remote peer.
    fn peer_addr(&self) -> SocketAddr;

    /// Returns the socket address of the local endpoint.
    fn local_addr(&self) -> SocketAddr;
}

/// A readable stream.
#[async_trait]
pub trait ReadStream: Send + Sync + 'static {
    /// Stops transport background tasks before a disconnection event is queued.
    /// Implementations may retain their registration until this half is dropped.
    fn close(&mut self) {}

    /// Supplies idle timeout updates for protocols that support them. Other protocols ignore it.
    fn set_idle_timeout(&mut self, _timeout: tokio::sync::watch::Receiver<std::time::Duration>) {}

    /// Fills the whole buffer with bytes in this stream.
    async fn read_exact(&mut self, buffer: &mut [u8]) -> io::Result<()>;

    /// Reads a single packet from this stream.
    ///
    /// The default uses length-prefixed framing; datagram transports override it.
    async fn receive<ReceivingPacket, SendingPacket, S, LS>(
        &mut self,
        serializer: Arc<S>,
        length_serializer: &LS,
    ) -> Result<ReceivingPacket, ReceiveError<S::DecodeError, LS>>
    where
        ReceivingPacket: Send + Sync + Debug + 'static,
        SendingPacket: Send + Sync + Debug + 'static,
        S: Serializer<ReceivingPacket, SendingPacket> + ?Sized,
        LS: PacketLengthSerializer,
    {
        FramedReader::new(self)
            .receive(serializer, length_serializer)
            .await
            .map(|(packet, _)| packet)
    }

    /// Reads a packet and its receive time.
    ///
    /// Built-in protocols capture the time before decoding (and before UDP queueing).
    /// The default preserves custom `receive` implementations and timestamps their completion;
    /// override this method to provide the transport's receive time.
    async fn receive_with_timestamp<ReceivingPacket, SendingPacket, S, LS>(
        &mut self,
        serializer: Arc<S>,
        length_serializer: &LS,
    ) -> Result<(ReceivingPacket, Instant), ReceiveError<S::DecodeError, LS>>
    where
        ReceivingPacket: Send + Sync + Debug + 'static,
        SendingPacket: Send + Sync + Debug + 'static,
        S: Serializer<ReceivingPacket, SendingPacket> + ?Sized,
        LS: PacketLengthSerializer,
    {
        let packet = self.receive(serializer, length_serializer).await?;
        Ok((packet, Instant::now()))
    }
}

pub(crate) struct FramedReader<'a, R: ?Sized> {
    read: &'a mut R,
}
impl<'a, R: ReadStream + ?Sized> FramedReader<'a, R> {
    pub(crate) const fn new(read: &'a mut R) -> Self {
        Self { read }
    }
    pub(crate) async fn receive<ReceivingPacket, SendingPacket, S, LS>(
        mut self,
        serializer: Arc<S>,
        length_serializer: &LS,
    ) -> Result<(ReceivingPacket, Instant), ReceiveError<S::DecodeError, LS>>
    where
        ReceivingPacket: Send + Sync + Debug + 'static,
        SendingPacket: Send + Sync + Debug + 'static,
        S: Serializer<ReceivingPacket, SendingPacket> + ?Sized,
        LS: PacketLengthSerializer,
    {
        let length = self.read_length(length_serializer).await?;
        if length > MAX_PACKET_SIZE.load(Ordering::Relaxed) {
            return Err(ReceiveError::PacketTooBig);
        }

        let mut payload = vec![0; length];
        self.read
            .read_exact(&mut payload)
            .await
            .map_err(ReceiveError::Io)?;
        let received_at = Instant::now();
        let packet = serializer
            .deserialize(&payload)
            .map_err(ReceiveError::Deserialization)?;
        Ok((packet, received_at))
    }

    async fn read_length<DecErr, LS>(
        &mut self,
        length_serializer: &LS,
    ) -> Result<usize, ReceiveError<DecErr, LS>>
    where
        DecErr: Error + Send + Sync,
        LS: PacketLengthSerializer,
    {
        let mut prefix = vec![0; LS::SIZE];
        let mut filled = 0;
        loop {
            self.read
                .read_exact(
                    prefix.get_mut(filled..).ok_or_else(|| {
                        ReceiveError::Io(io::Error::from(io::ErrorKind::InvalidData))
                    })?,
                )
                .await
                .map_err(ReceiveError::Io)?;
            match length_serializer.deserialize_packet_length(&prefix) {
                Ok(length) => return Ok(length),
                Err(PacketLengthDeserializationError::NeedMoreBytes(additional)) => {
                    filled = prefix.len();
                    prefix.resize(filled + additional, 0);
                }
                Err(PacketLengthDeserializationError::Err(error)) => {
                    return Err(ReceiveError::LengthDeserialization(error));
                }
            }
        }
    }
}

/// Reports transport and decoding failures, including an explicitly closed connection.
#[derive(thiserror::Error)]
pub enum ReceiveError<SerializationError, LS>
where
    SerializationError: Error + Send + Sync,
    LS: PacketLengthSerializer,
{
    /// Stops reception when the transport cannot supply a complete packet.
    #[error("Failed to receive packet: {0}")]
    Io(#[source] io::Error),
    /// Rejects a complete payload using the packet codec's error.
    #[error("Failed to decode packet: {0}")]
    Deserialization(#[source] SerializationError),
    /// Stops framing because the prefix cannot be decoded.
    #[error("Failed to decode packet length: {0}")]
    LengthDeserialization(#[source] LS::Error),
    /// Exceeds the process-wide [`MaxPacketSize`](crate::connection::MaxPacketSize).
    #[error("Packet exceeds the configured size limit")]
    PacketTooBig,
    /// Reports a failed connection attempt before packet reception starts.
    #[error("Failed to connect: {0}")]
    NoConnection(#[source] io::Error),
    /// Follows an explicit [`disconnect`](crate::connection::EcsConnection::disconnect) request.
    #[error("Connection was explicitly closed")]
    IntentionalDisconnection,
}

impl<SerializationError, LS> Debug for ReceiveError<SerializationError, LS>
where
    SerializationError: Error + Send + Sync,
    LS: PacketLengthSerializer,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "ReceiveError::Io({error:?})"),
            Self::Deserialization(error) => {
                write!(f, "ReceiveError::Deserialization({error:?})")
            }
            Self::LengthDeserialization(error) => {
                write!(f, "ReceiveError::LengthDeserialization({error:?})")
            }
            Self::PacketTooBig => write!(f, "ReceiveError::PacketTooBig"),
            Self::NoConnection(error) => write!(f, "ReceiveError::NoConnection({error:?})"),
            Self::IntentionalDisconnection => write!(f, "IntentionalDisconnection"),
        }
    }
}

/// A writeable stream.
#[async_trait]
pub trait WriteStream: Send + Sync + 'static {
    /// Writes the whole buffer to the stream.
    async fn write_all(&mut self, buffer: &[u8]) -> io::Result<()>;

    /// Writes a packet to this stream.
    ///
    /// The default serializes the payload and its length before writing either.
    ///
    /// # Errors
    /// Returns an error if payload encoding, length encoding or the transport write fails.
    async fn send<ReceivingPacket, SendingPacket, S, LS>(
        &mut self,
        packet: SendingPacket,
        serializer: Arc<S>,
        length_serializer: &LS,
    ) -> io::Result<()>
    where
        ReceivingPacket: Send + Sync + Debug + 'static,
        SendingPacket: Send + Sync + Debug + 'static,
        S: Serializer<ReceivingPacket, SendingPacket> + ?Sized,
        LS: PacketLengthSerializer,
    {
        let serialized = serializer
            .serialize(packet)
            .map_err(|err| io::Error::other(format!("Error serializing packet: {err}")))?;
        let mut buf = length_serializer
            .serialize_packet_length(serialized.len())
            .map_err(|err| io::Error::other(format!("Error serializing packet length: {err}")))?;
        buf.extend_from_slice(&serialized);
        self.write_all(&buf).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serializers::packet_length_serializer::LittleEndian;
    use std::sync::Mutex;

    #[derive(Default)]
    struct DecodeTime(Mutex<Option<Instant>>);

    impl Serializer<Vec<u8>, Vec<u8>> for DecodeTime {
        type EncodeError = io::Error;
        type DecodeError = io::Error;

        fn serialize(&self, packet: Vec<u8>) -> io::Result<Vec<u8>> {
            Ok(packet)
        }

        fn deserialize(&self, data: &[u8]) -> io::Result<Vec<u8>> {
            *self.0.lock().unwrap() = Some(Instant::now());
            Ok(data.to_vec())
        }
    }

    struct BufferedRead(io::Cursor<Vec<u8>>);

    #[async_trait]
    impl ReadStream for BufferedRead {
        async fn read_exact(&mut self, buffer: &mut [u8]) -> io::Result<()> {
            io::Read::read_exact(&mut self.0, buffer)
        }
    }

    struct ExtendedLength;

    impl PacketLengthSerializer for ExtendedLength {
        type Error = io::Error;
        const SIZE: usize = 1;

        fn serialize_packet_length(&self, length: usize) -> io::Result<Vec<u8>> {
            let length = u16::try_from(length).map_err(io::Error::other)?;
            let mut prefix = vec![255];
            prefix.extend(length.to_le_bytes());
            Ok(prefix)
        }

        fn deserialize_packet_length(
            &self,
            prefix: &[u8],
        ) -> Result<usize, PacketLengthDeserializationError<io::Error>> {
            match prefix {
                [255] => Err(PacketLengthDeserializationError::NeedMoreBytes(2)),
                [255, low, high] => Ok(u16::from_le_bytes([*low, *high]) as usize),
                [length] => Ok(*length as usize),
                _ => Err(PacketLengthDeserializationError::Err(io::Error::other(
                    "invalid prefix",
                ))),
            }
        }
    }

    #[tokio::test]
    async fn variable_prefixes_preserve_packet_boundaries() {
        let mut read = BufferedRead(io::Cursor::new(vec![255, 2, 0, 7, 8, 0, 1, 9]));
        let serializer = Arc::new(DecodeTime::default());
        for expected in [vec![7, 8], vec![], vec![9]] {
            let packet = read
                .receive(Arc::clone(&serializer), &ExtendedLength)
                .await
                .unwrap();
            assert_eq!(packet, expected);
        }
        assert_eq!(read.0.position(), 8);
    }

    #[tokio::test]
    async fn incomplete_extended_prefix_reports_transport_error() {
        let mut read = BufferedRead(io::Cursor::new(vec![255, 2]));
        let error = read
            .receive(Arc::new(DecodeTime::default()), &ExtendedLength)
            .await
            .unwrap_err();
        assert!(
            matches!(error, ReceiveError::Io(ref source) if source.kind() == io::ErrorKind::UnexpectedEof)
        );
        assert!(error.source().is_some());
    }

    #[cfg(any(feature = "protocol_tcp", feature = "protocol_udp"))]
    async fn check_timestamps<P: Protocol>()
    where
        P::Listener: Send + Sync + 'static,
    {
        async fn check(read: &mut impl ReadStream, write: &mut impl WriteStream) {
            let serializer = Arc::new(DecodeTime::default());
            let length = LittleEndian::<u32>::default();
            for payload in [vec![7], vec![]] {
                write
                    .send(payload.clone(), Arc::clone(&serializer), &length)
                    .await
                    .unwrap();
                let (packet, received_at) = read
                    .receive_with_timestamp(Arc::clone(&serializer), &length)
                    .await
                    .unwrap();
                assert_eq!(packet, payload);
                assert!(received_at <= serializer.0.lock().unwrap().unwrap());
            }
        }

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let listener = Arc::new(P::bind(([127, 0, 0, 1], 0).into()).await.unwrap());
            let (client, server) =
                tokio::join!(P::connect_to_server(listener.address()), listener.accept());
            let (client, server) = (client.unwrap(), server.unwrap());
            let pump = tokio::spawn(async move { while listener.accept().await.is_ok() {} });
            let (mut client_read, mut client_write) = client.into_split().await.unwrap();
            let (mut server_read, mut server_write) = server.into_split().await.unwrap();
            check(&mut server_read, &mut client_write).await;
            check(&mut client_read, &mut server_write).await;
            pump.abort();
        })
        .await
        .expect("timestamp test timed out");
    }

    #[cfg(feature = "protocol_tcp")]
    #[tokio::test]
    async fn tcp_timestamps_precede_decoding() {
        check_timestamps::<crate::protocols::tcp::TcpProtocol>().await;
    }

    #[cfg(feature = "protocol_udp")]
    #[tokio::test]
    async fn udp_timestamps_precede_decoding() {
        check_timestamps::<crate::protocols::udp::UdpProtocol>().await;
    }

    #[tokio::test]
    async fn timestamp_fallback_preserves_custom_receive() {
        struct CustomRead;

        #[async_trait]
        impl ReadStream for CustomRead {
            async fn read_exact(&mut self, _buffer: &mut [u8]) -> io::Result<()> {
                panic!("custom receive must not use stream framing")
            }

            async fn receive<ReceivingPacket, SendingPacket, S, LS>(
                &mut self,
                serializer: Arc<S>,
                _length_serializer: &LS,
            ) -> Result<ReceivingPacket, ReceiveError<S::DecodeError, LS>>
            where
                ReceivingPacket: Send + Sync + Debug + 'static,
                SendingPacket: Send + Sync + Debug + 'static,
                S: Serializer<ReceivingPacket, SendingPacket> + ?Sized,
                LS: PacketLengthSerializer,
            {
                serializer
                    .deserialize(&[7])
                    .map_err(ReceiveError::Deserialization)
            }
        }

        let serializer = Arc::new(DecodeTime::default());
        let (packet, received_at) = CustomRead
            .receive_with_timestamp(Arc::clone(&serializer), &LittleEndian::<u32>::default())
            .await
            .unwrap();
        assert_eq!(packet, vec![7]);
        assert!(received_at >= serializer.0.lock().unwrap().unwrap());
    }
}

#[cfg(test)]
mod send_tests {
    use super::*;
    use crate::serializers::packet_length_serializer::LittleEndian;

    #[derive(Default)]
    struct RecordingWriter {
        writes: Vec<Vec<u8>>,
    }
    #[async_trait]
    impl WriteStream for RecordingWriter {
        async fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.writes.push(bytes.to_vec());
            Ok(())
        }
    }

    struct PacketCodec {
        reject_encoding: bool,
    }
    impl Serializer<Vec<u8>, Vec<u8>> for PacketCodec {
        type EncodeError = io::Error;
        type DecodeError = io::Error;
        fn serialize(&self, packet: Vec<u8>) -> io::Result<Vec<u8>> {
            if self.reject_encoding {
                Err(io::Error::other("encoding rejected"))
            } else {
                Ok(packet)
            }
        }
        fn deserialize(&self, bytes: &[u8]) -> io::Result<Vec<u8>> {
            Ok(bytes.to_vec())
        }
    }

    #[tokio::test]
    async fn payload_encoding_failure_does_not_write() {
        let mut writer = RecordingWriter::default();
        let error = writer
            .send(
                vec![7],
                Arc::new(PacketCodec {
                    reject_encoding: true,
                }),
                &LittleEndian::<u32>::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(
            error.to_string(),
            "Error serializing packet: encoding rejected"
        );
        assert!(writer.writes.is_empty());
    }

    #[tokio::test]
    async fn length_encoding_failure_does_not_write() {
        let mut writer = RecordingWriter::default();
        let error = writer
            .send(
                vec![7; 256],
                Arc::new(PacketCodec {
                    reject_encoding: false,
                }),
                &LittleEndian::<u8>::default(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "Error serializing packet length: The packet is too large (length: 256, max_length: 255)");
        assert!(writer.writes.is_empty());
    }

    #[tokio::test]
    async fn successful_encoding_writes_length_then_payload() {
        let mut writer = RecordingWriter::default();
        writer
            .send(
                vec![7, 8],
                Arc::new(PacketCodec {
                    reject_encoding: false,
                }),
                &LittleEndian::<u16>::default(),
            )
            .await
            .unwrap();
        assert_eq!(writer.writes, [vec![2, 0, 7, 8]]);
    }
}
