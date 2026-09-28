use super::*;
use crate::{
    connection::ReceiveLimits,
    protocol::{ClientStream, Listener, NetworkStream, PacketReader, PacketWriter},
    serializers::{packet_length_serializer::LittleEndian, serializer::Serializer},
};
use std::{io, sync::Arc, time::Duration};
use tokio::net::UdpSocket;

struct Bytes;
impl Serializer<Vec<u8>, Vec<u8>> for Bytes {
    type EncodeError = io::Error;
    type DecodeError = io::Error;
    fn serialize(&self, packet: Vec<u8>) -> io::Result<Vec<u8>> {
        Ok(packet)
    }
    fn deserialize(&self, bytes: &[u8]) -> io::Result<Vec<u8>> {
        Ok(bytes.to_vec())
    }
}
type Length = LittleEndian<u32>;

#[tokio::test]
async fn client_setup_and_close_send_nothing() {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let client = tokio::time::timeout(
        Duration::from_millis(100),
        UdpClientStream::connect(socket.local_addr().unwrap()),
    )
    .await
    .expect("UDP setup must not wait for a handshake")
    .unwrap();
    let (read, write) = client.into_split().await.unwrap();
    drop(read);
    drop(write);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), socket.recv(&mut [0; 256]))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn wire_is_exact_serializer_output_in_both_directions() {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let client = tokio::time::timeout(
        Duration::from_millis(100),
        UdpClientStream::connect(socket.local_addr().unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    let (mut read, mut write) = client.into_split().await.unwrap();
    for payload in [
        vec![],
        vec![0],
        b"SLN2\x03".to_vec(),
        vec![255],
        vec![9; 1200],
    ] {
        write
            .send(payload.clone(), Arc::new(Bytes), &Length::default())
            .await
            .unwrap();
        let mut buffer = [0; 2048];
        let (len, address) =
            tokio::time::timeout(Duration::from_secs(1), socket.recv_from(&mut buffer))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(&buffer[..len], payload.as_slice());
        socket.send_to(&payload, address).await.unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            read.receive(
                Arc::new(Bytes),
                &Length::default(),
                &ReceiveLimits::default(),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result, payload);
    }
}

#[tokio::test]
async fn first_empty_server_datagram_is_preserved() {
    let listener = UdpNetworkListener::bind("127.0.0.1:0".parse().unwrap(), UdpOptions::DEFAULT)
        .await
        .unwrap();
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    socket.send_to(&[], listener.address()).await.unwrap();
    let (mut read, _write) = tokio::time::timeout(Duration::from_millis(100), listener.accept())
        .await
        .expect("first datagram must create a local peer")
        .unwrap()
        .into_split()
        .await
        .unwrap();
    assert_eq!(
        read.receive(
            Arc::new(Bytes),
            &Length::default(),
            &ReceiveLimits::default()
        )
        .await
        .unwrap(),
        Vec::<u8>::new()
    );
}
