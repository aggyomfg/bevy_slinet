use super::*;
use crate::serializers::packet_length_serializer::LittleEndian;
use crate::serializers::serializer::Serializer;
use std::io;
use std::sync::Arc;

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
    let mut writer = FramedWriter::new(RecordingWriter::default());
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
    assert!(writer.get_ref().writes.is_empty());
}

#[tokio::test]
async fn length_encoding_failure_does_not_write() {
    let mut writer = FramedWriter::new(RecordingWriter::default());
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
    assert_eq!(
        error.to_string(),
        "Error serializing packet length: The packet is too large (length: 256, max_length: 255)"
    );
    assert!(writer.get_ref().writes.is_empty());
}

#[tokio::test]
async fn successful_encoding_writes_length_then_payload() {
    let mut writer = FramedWriter::new(RecordingWriter::default());
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
    assert_eq!(writer.get_ref().writes, [vec![2, 0, 7, 8]]);
}
