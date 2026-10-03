#![allow(clippy::unwrap_used, reason = "Tests fail on unexpected I/O errors")]

#[cfg(any(feature = "protocol_tcp", feature = "protocol_udp"))]
mod sockets {
    use bevy_slinet::{
        connection::ReceiveLimits,
        protocol::{Listener, NetworkStream, PacketReader, PacketWriter},
        serializers::{packet_length_serializer::LittleEndian, serializer::Serializer},
    };
    use std::{io, sync::Arc, time::Duration};

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

    #[cfg(feature = "protocol_tcp")]
    #[tokio::test]
    async fn configured_tcp_sockets_keep_options_and_framing() {
        use bevy_slinet::protocols::tcp::{TcpNetworkListener, TcpNetworkStream};
        use tokio::net::{TcpListener, TcpStream};
        tokio::time::timeout(Duration::from_secs(5), async {
            let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = socket.local_addr().unwrap();
            let listener = TcpNetworkListener::from_listener(socket)
                .unwrap()
                .with_nodelay(true);
            assert_eq!(listener.address(), address);
            let socket = TcpStream::connect(address).await.unwrap();
            socket.set_nodelay(true).unwrap();
            let client = TcpNetworkStream::from_stream(socket).unwrap();
            assert!(client.socket().nodelay().unwrap());
            let server = listener.accept().await.unwrap();
            assert!(server.socket().nodelay().unwrap());
            assert_eq!(client.local_addr(), server.peer_addr());
            let (client_reader, mut writer) = client.into_split().await.unwrap();
            let (mut reader, server_writer) = server.into_split().await.unwrap();
            writer
                .send(
                    vec![1, 2, 3],
                    Arc::new(Bytes),
                    &LittleEndian::<u32>::default(),
                )
                .await
                .unwrap();
            assert_eq!(
                reader
                    .receive(
                        Arc::new(Bytes),
                        &LittleEndian::<u32>::default(),
                        &ReceiveLimits::new(32)
                    )
                    .await
                    .unwrap(),
                vec![1, 2, 3]
            );
            drop((client_reader, writer, reader, server_writer));
        })
        .await
        .unwrap();
    }

    #[cfg(feature = "protocol_udp")]
    #[tokio::test]
    async fn configured_udp_sockets_keep_bindings_and_deliver_packets() {
        use bevy_slinet::protocols::udp::{UdpClientStream, UdpNetworkListener, UdpOptions};
        use tokio::net::UdpSocket;
        tokio::time::timeout(Duration::from_secs(5), async {
            let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let address = socket.local_addr().unwrap();
            let listener = UdpNetworkListener::from_socket(socket, UdpOptions::DEFAULT).unwrap();
            let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let local = socket.local_addr().unwrap();
            socket.connect(address).await.unwrap();
            let client = UdpClientStream::from_socket(socket, UdpOptions::DEFAULT).unwrap();
            assert_eq!(client.local_addr(), local);
            let (client_reader, mut writer) = client.into_split().await.unwrap();
            writer
                .send(vec![4, 5], Arc::new(Bytes), &LittleEndian::<u32>::default())
                .await
                .unwrap();
            let (mut reader, server_writer) = {
                let server = listener.accept().await.unwrap();
                assert_eq!(server.peer_addr(), local);
                server.into_split().await.unwrap()
            };
            assert_eq!(
                reader
                    .receive(
                        Arc::new(Bytes),
                        &LittleEndian::<u32>::default(),
                        &ReceiveLimits::new(32)
                    )
                    .await
                    .unwrap(),
                vec![4, 5]
            );
            drop((client_reader, writer, reader, server_writer));
        })
        .await
        .unwrap();
    }

    #[cfg(feature = "protocol_udp")]
    #[tokio::test]
    async fn udp_constructors_reject_incompatible_socket_state_and_options() {
        use bevy_slinet::protocols::udp::{UdpClientStream, UdpNetworkListener, UdpOptions};
        use tokio::net::UdpSocket;
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        assert!(UdpClientStream::from_socket(socket, UdpOptions::DEFAULT).is_err());
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        socket.connect("127.0.0.1:1234").await.unwrap();
        assert!(
            matches!(UdpNetworkListener::from_socket(socket, UdpOptions::DEFAULT), Err(e) if e.kind() == io::ErrorKind::InvalidInput)
        );
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let options = UdpOptions {
            max_datagram_size: usize::MAX,
            ..UdpOptions::DEFAULT
        };
        assert!(
            matches!(UdpNetworkListener::from_socket(socket, options), Err(e) if e.kind() == io::ErrorKind::InvalidInput)
        );
    }
}
