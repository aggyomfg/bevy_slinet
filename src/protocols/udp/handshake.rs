use super::settings::{ValidatedOptions, BUFFER_SIZE};
use super::wire::{Control, Cookie, Frame, HandshakeFrame, HandshakeKind, Nonce, Payload};
use std::{
    io::{self, ErrorKind},
    time::Duration,
};
use tokio::{net::UdpSocket, time::Instant as Clock};

/// Retry delay for one handshake leg. The first datagram is sent immediately.
struct RetryTiming {
    base: Duration,
    max: Duration,
    jitter: Duration,
    random: u64,
}
impl RetryTiming {
    fn new(options: ValidatedOptions, nonce: Nonce) -> Self {
        let mut seed = [0; 8];
        seed.copy_from_slice(&nonce.as_bytes()[..8]);
        Self {
            base: options.initial_retry_interval(),
            max: options.max_retry_interval(),
            jitter: options.retry_jitter(),
            random: u64::from_le_bytes(seed) | 1,
        }
    }

    const fn reset(&mut self, options: ValidatedOptions) {
        self.base = options.initial_retry_interval();
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "The shifted random value has at most 53 bits"
    )]
    fn next_delay(&mut self) -> Duration {
        self.random ^= self.random << 13;
        self.random ^= self.random >> 7;
        self.random ^= self.random << 17;
        let delay = self.base
            + self
                .jitter
                .mul_f64((self.random >> 11) as f64 / (1u64 << 53) as f64);
        self.base = self.base.checked_mul(2).unwrap_or(self.max).min(self.max);
        delay
    }
}

pub(super) struct Handshake<'a> {
    socket: &'a UdpSocket,
}
impl<'a> Handshake<'a> {
    pub(super) const fn new(socket: &'a UdpSocket) -> Self {
        Self { socket }
    }

    pub(super) async fn connect(self, options: ValidatedOptions) -> io::Result<(Cookie, bool)> {
        let deadline = Clock::now() + options.connect_timeout();
        // Socket sends can wait for writable readiness too. The absolute deadline
        // covers every await in both legs, including the very first HELLO.
        tokio::time::timeout_at(deadline, self.connect_until(options, deadline))
            .await
            .map_err(|_| io::Error::new(ErrorKind::TimedOut, "UDP handshake timed out"))?
    }

    async fn connect_until(
        self,
        options: ValidatedOptions,
        deadline: Clock,
    ) -> io::Result<(Cookie, bool)> {
        let socket = self.socket;
        let nonce = Nonce::generate()?;
        let hello = HandshakeFrame::hello(nonce).encode();
        let mut selected: Option<Cookie> = None;
        let mut buffer = vec![0; BUFFER_SIZE];
        let mut timing = RetryTiming::new(options, nonce);
        socket.send(&hello).await?;
        let mut retry_at = Clock::now() + timing.next_delay();
        loop {
            tokio::select! {
                biased;
                () = tokio::time::sleep_until(deadline) => {
                    return Err(io::Error::new(ErrorKind::TimedOut, "UDP handshake timed out"));
                }
                () = tokio::time::sleep_until(retry_at) => {
                    let request = selected.map_or(hello, |cookie| HandshakeFrame::confirm(cookie).encode());
                    socket.send(&request).await?;
                    retry_at = Clock::now() + timing.next_delay();
                }
                result = socket.peek(&mut buffer) => {
                    let len = result?;
                    let bytes = buffer.get(..len).ok_or_else(|| io::Error::from(ErrorKind::InvalidData))?;
                    if let Some(HandshakeFrame { kind, cookie }) = HandshakeFrame::parse(bytes) {
                        socket.recv(&mut buffer).await?;
                        if kind == HandshakeKind::Challenge && cookie.nonce == nonce && selected.is_none() {
                            // The first challenge pins the session. A later challenge cannot
                            // switch the client to a different cookie after a lost ACCEPT.
                            selected = Some(cookie);
                            timing.reset(options);
                            socket.send(&HandshakeFrame::confirm(cookie).encode()).await?;
                            retry_at = Clock::now() + timing.next_delay();
                        }
                        continue;
                    }
                    if let (Some(cookie), Some(Frame { session, payload })) = (selected, Frame::parse(bytes)) {
                        if session == cookie.mac {
                            match payload {
                                Payload::Control(Control::Accept) => { socket.recv(&mut buffer).await?; return Ok((cookie, true)); }
                                Payload::Data(_) => return Ok((cookie, false)), // Preserve early application data for the read half.
                                Payload::Control(Control::Disconnect) => return Err(io::Error::new(ErrorKind::ConnectionRefused, "UDP connection refused")),
                                Payload::Control(Control::Keepalive) => {},
                            }
                        }
                    }
                    socket.recv(&mut buffer).await?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::settings::UdpOptions;
    use super::*;

    #[test]
    fn retry_jitter_stays_inside_each_backoff_interval() {
        let options = ValidatedOptions::new(UdpOptions::DEFAULT).unwrap();
        let mut timing = RetryTiming::new(options, Nonce::from_bytes([17; 16]));
        let mut previous_jitter = None;
        let mut varied = false;
        for base in [1, 2, 4, 4, 4, 4, 4, 4] {
            let delay = timing.next_delay();
            let base = Duration::from_secs(base);
            assert!(delay >= base && delay <= base + options.retry_jitter());
            let jitter = delay.checked_sub(base).unwrap();
            varied |= previous_jitter.is_some_and(|previous| previous != jitter);
            previous_jitter = Some(jitter);
        }
        assert!(varied);
        timing.reset(options);
        let delay = timing.next_delay();
        assert!(delay >= options.initial_retry_interval());
        assert!(delay <= options.initial_retry_interval() + options.retry_jitter());
    }
}
