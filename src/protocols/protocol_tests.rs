use super::*;
use crate::connection::ReceiveLimits;
use crate::serializers::packet_length_serializer::LittleEndian;
use crate::serializers::packet_length_serializer::PacketLengthDeserializationError;
use crate::serializers::serializer::Serializer;
use crate::PacketLengthSerializer;
use bevy::platform::time::Instant;
use std::error::Error;
use std::fmt::Debug;
use std::io;
use std::sync::Arc;
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
async fn receive_limits_are_independent_and_checked_before_payload_allocation() {
    let narrow = ReceiveLimits::new(1);
    let wide = ReceiveLimits::new(2);
    let serializer = Arc::new(DecodeTime::default());
    let mut rejected = FramedReader::new(BufferedRead(io::Cursor::new(vec![2, 7, 8])));
    let error = rejected
        .receive(Arc::clone(&serializer), &ExtendedLength, &narrow)
        .await
        .unwrap_err();
    assert!(matches!(error, ReceiveError::PacketTooBig));
    assert_eq!(rejected.get_ref().0.position(), 1);
    let mut allowed = FramedReader::new(BufferedRead(io::Cursor::new(vec![2, 7, 8, 1, 9])));
    assert_eq!(
        allowed
            .receive(Arc::clone(&serializer), &ExtendedLength, &wide)
            .await
            .unwrap(),
        vec![7, 8]
    );
    wide.set_max_packet_size(0);
    let error = allowed
        .receive(serializer, &ExtendedLength, &wide)
        .await
        .unwrap_err();
    assert!(matches!(error, ReceiveError::PacketTooBig));
    assert_eq!(allowed.get_ref().0.position(), 4);
    assert_eq!(narrow.max_packet_size(), 1);
}

#[tokio::test]
async fn variable_prefixes_preserve_packet_boundaries() {
    let mut read = FramedReader::new(BufferedRead(io::Cursor::new(vec![
        255, 2, 0, 7, 8, 0, 1, 9,
    ])));
    let serializer = Arc::new(DecodeTime::default());
    for expected in [vec![7, 8], vec![], vec![9]] {
        let packet = read
            .receive(
                Arc::clone(&serializer),
                &ExtendedLength,
                &ReceiveLimits::default(),
            )
            .await
            .unwrap();
        assert_eq!(packet, expected);
    }
    assert_eq!(read.get_ref().0.position(), 8);
}

#[tokio::test]
async fn incomplete_extended_prefix_reports_transport_error() {
    let mut read = FramedReader::new(BufferedRead(io::Cursor::new(vec![255, 2])));
    let error = read
        .receive(
            Arc::new(DecodeTime::default()),
            &ExtendedLength,
            &ReceiveLimits::default(),
        )
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
    async fn check(read: &mut impl PacketReader, write: &mut impl PacketWriter) {
        let serializer = Arc::new(DecodeTime::default());
        let length = LittleEndian::<u32>::default();
        for payload in [vec![7], vec![]] {
            write
                .send(payload.clone(), Arc::clone(&serializer), &length)
                .await
                .unwrap();
            let (packet, received_at) = read
                .receive_with_timestamp(Arc::clone(&serializer), &length, &ReceiveLimits::default())
                .await
                .unwrap();
            assert_eq!(packet, payload);
            assert!(received_at <= serializer.0.lock().unwrap().unwrap());
        }
    }

    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        let listener = Arc::new(P::bind(([127, 0, 0, 1], 0).into()).await.unwrap());
        let (client, server) = tokio::join!(
            async {
                let client = P::connect_to_server(listener.address()).await.unwrap();
                let (read, mut write) = client.into_split().await.unwrap();
                write
                    .send(
                        vec![0],
                        Arc::new(DecodeTime::default()),
                        &LittleEndian::<u32>::default(),
                    )
                    .await
                    .unwrap();
                (read, write)
            },
            listener.accept()
        );
        let server = server.unwrap();
        let pump = tokio::spawn(async move { while listener.accept().await.is_ok() {} });
        let (mut client_read, mut client_write) = client;
        let (mut server_read, mut server_write) = server.into_split().await.unwrap();
        server_read
            .receive(
                Arc::new(DecodeTime::default()),
                &LittleEndian::<u32>::default(),
                &ReceiveLimits::default(),
            )
            .await
            .unwrap();
        check(&mut server_read, &mut client_write).await;
        check(&mut client_read, &mut server_write).await;
        pump.abort();
    })
    .await
    .expect("timestamp test timed out");
}

#[cfg(feature = "protocol_tcp")]
#[tokio::test]
async fn tcp_receive_limit_rejects_header_without_waiting_for_payload() {
    use crate::protocols::tcp::TcpProtocol;

    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let listener = TcpProtocol::bind(([127, 0, 0, 1], 0).into()).await.unwrap();
        let (client, server) = tokio::join!(
            TcpProtocol::connect_to_server(listener.address()),
            listener.accept()
        );
        let (_, mut writer) = client.unwrap().into_split().await.unwrap();
        let (mut reader, _server_writer) = server.unwrap().into_split().await.unwrap();
        let limits = ReceiveLimits::new(2);
        let serializer = Arc::new(DecodeTime::default());
        let lengths = LittleEndian::<u32>::default();
        writer
            .send(vec![7, 8], Arc::clone(&serializer), &lengths)
            .await
            .unwrap();
        assert_eq!(
            reader
                .receive_with_timestamp(Arc::clone(&serializer), &lengths, &limits)
                .await
                .unwrap()
                .0,
            vec![7, 8]
        );
        limits.set_max_packet_size(1);
        *serializer.0.lock().unwrap() = None;
        writer
            .get_mut()
            .write_all(&2u32.to_le_bytes())
            .await
            .unwrap();
        let error = reader
            .receive_with_timestamp(Arc::clone(&serializer), &lengths, &limits)
            .await
            .unwrap_err();
        assert!(matches!(error, ReceiveError::PacketTooBig));
        assert!(serializer.0.lock().unwrap().is_none());
    })
    .await
    .expect("TCP receive-limit test timed out");
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
    impl PacketReader for CustomRead {
        async fn receive<ReceivingPacket, SendingPacket, S, LS>(
            &mut self,
            serializer: Arc<S>,
            _length_serializer: &LS,
            _limits: &ReceiveLimits,
        ) -> Result<ReceivingPacket, ReceiveError<S::DecodeError, LS::Error>>
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
        .receive_with_timestamp(
            Arc::clone(&serializer),
            &LittleEndian::<u32>::default(),
            &ReceiveLimits::default(),
        )
        .await
        .unwrap();
    assert_eq!(packet, vec![7]);
    assert!(received_at >= serializer.0.lock().unwrap().unwrap());
}
