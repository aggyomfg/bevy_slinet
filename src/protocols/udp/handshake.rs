use super::settings::{BUFFER_SIZE, PROBE_INTERVAL};
use super::wire::{Control, Cookie, Frame, HandshakeFrame, HandshakeKind, Nonce, Payload};
use std::io::{self, ErrorKind};
use tokio::{net::UdpSocket, time::MissedTickBehavior};

pub(super) struct Handshake<'a> {
    socket: &'a UdpSocket,
}
impl<'a> Handshake<'a> {
    pub(super) fn new(socket: &'a UdpSocket) -> Self {
        Self { socket }
    }
    pub(super) async fn connect(self) -> io::Result<Cookie> {
        let socket = self.socket;
        let nonce = Nonce::generate()?;
        let hello = HandshakeFrame::hello(nonce).encode();
        let mut selected: Option<Cookie> = None;
        let mut buffer = vec![0; BUFFER_SIZE];
        let mut retry = tokio::time::interval(PROBE_INTERVAL);
        retry.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = retry.tick() => {
                    let request = selected.map(|cookie| HandshakeFrame::confirm(cookie).encode()).unwrap_or(hello);
                    socket.send(&request).await?;
                }
                result = socket.peek(&mut buffer) => {
                    let len = result?;
                    if let Some(HandshakeFrame { kind, cookie }) = HandshakeFrame::parse(&buffer[..len]) {
                        socket.recv(&mut buffer).await?;
                        if kind == HandshakeKind::Challenge && cookie.nonce == nonce && selected.is_none_or(|old| cookie.generation > old.generation) {
                            selected = Some(cookie);
                            socket.send(&HandshakeFrame::confirm(cookie).encode()).await?;
                        }
                        continue;
                    }
                    if let (Some(cookie), Some(Frame { session, payload })) = (selected, Frame::parse(&buffer[..len])) {
                        if session == cookie.mac {
                            match payload {
                                Payload::Control(Control::Accept) => { socket.recv(&mut buffer).await?; return Ok(cookie); }
                                Payload::Data(_) => return Ok(cookie), // Preserve early application data for the read half.
                                Payload::Control(Control::Disconnect) => return Err(io::Error::new(ErrorKind::ConnectionRefused, "UDP connection refused")),
                                _ => {},
                            }
                        }
                    }
                    socket.recv(&mut buffer).await?;
                }
            }
        }
    }
}
