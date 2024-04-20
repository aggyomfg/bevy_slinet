//! UDP protocol implementation based on [`tokio::net`]. You can enable it by adding `protocol_udp` feature.

use std::future::Future;
use std::io;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use async_trait::async_trait;
use dashmap::DashMap;
use futures::task::AtomicWaker;
use tokio::net::UdpSocket;
use tokio::sync::Mutex;

use crate::protocol::{
    ClientStream, Listener, NetworkStream, ReadStream, ServerStream, WriteStream,
};
use crate::Protocol;

const BUFFER_SIZE: usize = u16::MAX as usize;

/// UDP protocol.
pub struct UdpProtocol;

#[async_trait]
impl Protocol for UdpProtocol {
    type Listener = UdpNetworkListener;
    type ServerStream = UdpServerStream;
    type ClientStream = UdpClientStream;

    async fn bind(addr: SocketAddr) -> std::io::Result<Self::Listener> {
        Ok(UdpNetworkListener {
            socket: Arc::new(UdpSocket::bind(addr).await?),
            tasks: DashMap::new(),
        })
    }
}

struct Inner {
    waker: AtomicWaker,
    bytes: Mutex<Vec<u8>>,
}

#[derive(Clone)]
struct UdpRead(Arc<Inner>);

/// A UDP listener.
pub struct UdpNetworkListener {
    socket: Arc<UdpSocket>,
    tasks: DashMap<SocketAddr, UdpRead>,
}

#[async_trait]
impl Listener for UdpNetworkListener {
    type Stream = UdpServerStream;

    async fn accept(&self) -> std::io::Result<UdpServerStream> {
        let mut buf = [0; BUFFER_SIZE];
        loop {
            let (bytes, address) = self.socket.recv_from(&mut buf).await?;
            let bytes = &buf[..bytes];
            if let Some(task) = self.tasks.get(&address) {
                {
                    let mut task_bytes = task.0.bytes.lock().await;
                    task_bytes.extend(bytes);
                }
                task.0.waker.wake();
            } else {
                let new_task = UdpRead(Arc::new(Inner {
                    waker: AtomicWaker::new(),
                    bytes: Mutex::new(Vec::new()),
                }));
                self.tasks.insert(address, new_task.clone());
                return Ok(UdpServerStream {
                    task: new_task,
                    peer_addr: address,
                    socket: Arc::clone(&self.socket),
                });
            }
        }
    }

    fn address(&self) -> SocketAddr {
        self.socket.local_addr().unwrap()
    }

    fn handle_disconnection(&self, peer_addr: SocketAddr) {
        self.tasks.remove(&peer_addr);
    }
}

/// A UDP server stream that contains cached bytes and a task waker.
pub struct UdpServerStream {
    task: UdpRead,
    peer_addr: SocketAddr,
    socket: Arc<UdpSocket>,
}

#[async_trait]
impl NetworkStream for UdpServerStream {
    type ReadHalf = UdpServerReadHalf;
    type WriteHalf = UdpServerWriteHalf;

    async fn into_split(self) -> io::Result<(Self::ReadHalf, Self::WriteHalf)> {
        let peer_addr = self.peer_addr();
        Ok((
            UdpServerReadHalf(self.task.clone()),
            UdpServerWriteHalf {
                peer_addr,
                socket: self.socket,
            },
        ))
    }

    fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr().unwrap()
    }
}

/// The read half of [`UdpServerStream`].
pub struct UdpServerReadHalf(UdpRead);

#[async_trait]
impl ReadStream for UdpServerReadHalf {
    fn read_exact<'life0, 'life1, 'async_trait>(
        &'life0 mut self,
        buffer: &'life1 mut [u8],
    ) -> Pin<Box<dyn Future<Output = Result<(), std::io::Error>> + std::marker::Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(UdpReadTask {
            read: self.0.clone(),
            buffer,
        })
    }
}

/// A future that tries to read bytes from cache, and receives additional bytes if needed.
/// [`UdpSocket::recv`] discards bytes that are not needed and there's no way to save them without buffering.
pub struct UdpReadTask<'a> {
    read: UdpRead,
    buffer: &'a mut [u8],
}

impl Future for UdpReadTask<'_> {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let UdpReadTask { read, buffer } = &mut *self;
        let mut bytes = read.0.bytes.try_lock().unwrap();

        // quick check to avoid registration if already done.
        if bytes.len() >= buffer.len() {
            buffer.copy_from_slice(&bytes[..buffer.len()]);
            *bytes = bytes[buffer.len()..].to_vec();
            return Poll::Ready(Ok(()));
        }

        read.0.waker.register(cx.waker());

        if bytes.len() >= buffer.len() {
            buffer.copy_from_slice(&bytes[..buffer.len()]);
            *bytes = bytes[buffer.len()..].to_vec();
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}

/// A write half of [`UdpServerStream`];
pub struct UdpServerWriteHalf {
    peer_addr: SocketAddr,
    socket: Arc<UdpSocket>,
}

#[async_trait]
impl WriteStream for UdpServerWriteHalf {
    async fn write_all(&mut self, buffer: &[u8]) -> std::io::Result<()> {
        self.socket
            .send_to(buffer, self.peer_addr)
            .await
            .and_then(|i| assert_all(i, buffer))
    }
}

impl ServerStream for UdpServerStream {}

/// A UDP client stream.
pub struct UdpClientStream {
    socket: UdpSocket,
    peer_addr: SocketAddr,
}

#[async_trait]
impl NetworkStream for UdpClientStream {
    type ReadHalf = UdpClientReadHalf;
    type WriteHalf = UdpClientWriteHalf;

    async fn into_split(mut self) -> io::Result<(Self::ReadHalf, Self::WriteHalf)> {
        let std_socket = self.socket.into_std()?;
        let std_socket2 = std_socket.try_clone()?;
        let read_socket = UdpSocket::from_std(std_socket)?;
        let write_socket = UdpSocket::from_std(std_socket2)?;
        let write = UdpClientWriteHalf {
            socket: write_socket,
        };
        let read = UdpClientReadHalf {
            socket: read_socket,
            buffer: Vec::new(),
        };
        Ok((read, write))
    }

    fn peer_addr(&self) -> SocketAddr {
        self.peer_addr // self.0.peer_addr().unwrap(). Tokio added it in https://github.com/tokio-rs/tokio/pull/4362 and then reverted in https://github.com/tokio-rs/tokio/pull/4392
    }

    fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr().unwrap()
    }
}

#[async_trait]
impl ClientStream for UdpClientStream {
    async fn connect(addr: SocketAddr) -> std::io::Result<Self>
    where
        Self: Sized,
    {
        let socket = UdpSocket::bind("127.0.0.1:0").await?;
        socket.connect(addr).await?;

        // TODO remove this
        let std_socket = socket.into_std().unwrap();
        let peer_addr = std_socket.peer_addr().unwrap();
        let socket = UdpSocket::from_std(std_socket).unwrap();

        // socket.connect and socket.send is not enough to handle ConnectionRefused, but 2 sends is
        socket.send(&[]).await?;
        socket.send(&[]).await?;
        Ok(UdpClientStream { socket, peer_addr })
    }
}

/// A read half of [`UdpClientStream`].
pub struct UdpClientReadHalf {
    socket: UdpSocket,
    buffer: Vec<u8>,
}

#[async_trait]
impl ReadStream for UdpClientReadHalf {
    async fn read_exact(&mut self, buffer: &mut [u8]) -> std::io::Result<()> {
        loop {
            if self.buffer.len() >= buffer.len() {
                buffer.copy_from_slice(&self.buffer[..buffer.len()]);
                self.buffer = self.buffer[buffer.len()..].to_vec();
                return Ok(());
            }
            let mut buf = [0; BUFFER_SIZE];
            let read = self.socket.recv(&mut buf).await?;
            self.buffer.extend(&buf[..read]);
        }
    }
}

/// A write half of [`UdpClientStream`].
pub struct UdpClientWriteHalf {
    socket: UdpSocket,
}

impl AsRef<UdpSocket> for UdpClientWriteHalf {
    fn as_ref(&self) -> &UdpSocket {
        &self.socket
    }
}

#[async_trait]
impl WriteStream for UdpClientWriteHalf {
    async fn write_all(&mut self, buffer: &[u8]) -> std::io::Result<()> {
        self.socket
            .send(buffer)
            .await
            .and_then(|i| assert_all(i, buffer))
    }
}

fn assert_all(i: usize, buf: &[u8]) -> io::Result<()> {
    if i == buf.len() {
        Ok(())
    } else {
        Err(io::Error::from(ErrorKind::BrokenPipe))
    }
}
