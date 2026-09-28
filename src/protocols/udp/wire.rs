//! SLN2 encoding and stateless, address-bound handshake cookies.
use std::io;
use std::net::SocketAddr;
use std::sync::Mutex;
use tokio::time::Instant;

const MAGIC: &[u8; 4] = b"SLN2";
pub(super) const HEADER: usize = 37;
pub(super) const HANDSHAKE_SIZE: usize = 69;

/// Discriminants are SLN2 wire values, independent of declaration order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, strum::FromRepr)]
#[repr(u8)]
enum Tag {
    Data = 1,
    Keepalive = 2,
    Disconnect = 3,
    Hello = 4,
    Challenge = 5,
    Confirm = 6,
    Accept = 7,
}

struct RandomBytes;
impl RandomBytes {
    fn generate<const N: usize>() -> io::Result<[u8; N]> {
        let mut bytes = [0; N];
        getrandom::fill(&mut bytes).map_err(|err| io::Error::other(err.to_string()))?;
        Ok(bytes)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Nonce([u8; 16]);
impl Nonce {
    pub fn generate() -> io::Result<Self> {
        RandomBytes::generate().map(Self)
    }
    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }
    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Session([u8; 32]);
impl Session {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Control {
    Keepalive,
    Disconnect,
    Accept,
}
impl Control {
    const fn tag(self) -> Tag {
        match self {
            Self::Keepalive => Tag::Keepalive,
            Self::Disconnect => Tag::Disconnect,
            Self::Accept => Tag::Accept,
        }
    }
    pub fn encode(self, session: Session) -> [u8; HEADER] {
        let mut bytes = [0; HEADER];
        bytes[..4].copy_from_slice(MAGIC);
        bytes[4] = self.tag() as u8;
        bytes[5..].copy_from_slice(session.as_bytes());
        bytes
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Payload<'a> {
    Data(&'a [u8]),
    Control(Control),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Frame<'a> {
    pub session: Session,
    pub payload: Payload<'a>,
}
impl<'a> Frame<'a> {
    pub const fn data(session: Session, payload: &'a [u8]) -> Self {
        Self {
            session,
            payload: Payload::Data(payload),
        }
    }
    pub fn encode(self) -> Vec<u8> {
        match self.payload {
            Payload::Control(control) => control.encode(self.session).to_vec(),
            Payload::Data(payload) => {
                let mut bytes = Vec::with_capacity(HEADER + payload.len());
                bytes.extend_from_slice(MAGIC);
                bytes.push(Tag::Data as u8);
                bytes.extend_from_slice(self.session.as_bytes());
                bytes.extend_from_slice(payload);
                bytes
            }
        }
    }
    /// Rejects unknown tags, nonempty control payloads and invalid datagram sizes.
    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        if !(HEADER..=super::MAX_DATAGRAM_SIZE).contains(&bytes.len()) || bytes.get(..4)? != MAGIC {
            return None;
        }
        let tag = Tag::from_repr(*bytes.get(4)?)?;
        let payload = match tag {
            Tag::Data => Payload::Data(bytes.get(HEADER..)?),
            Tag::Keepalive | Tag::Disconnect | Tag::Accept if bytes.len() == HEADER => {
                Payload::Control(match tag {
                    Tag::Keepalive => Control::Keepalive,
                    Tag::Disconnect => Control::Disconnect,
                    Tag::Accept => Control::Accept,
                    Tag::Data | Tag::Hello | Tag::Challenge | Tag::Confirm => return None,
                })
            }
            Tag::Keepalive
            | Tag::Disconnect
            | Tag::Hello
            | Tag::Challenge
            | Tag::Confirm
            | Tag::Accept => return None,
        };
        Some(Self {
            session: Session::from_bytes(bytes.get(5..HEADER)?.try_into().ok()?),
            payload,
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Cookie {
    pub nonce: Nonce,
    pub epoch: u64,
    pub generation: u64,
    pub mac: Session,
}
impl Cookie {
    pub const fn hello(nonce: Nonce) -> Self {
        Self {
            nonce,
            epoch: 0,
            generation: 0,
            mac: Session::from_bytes([0; 32]),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HandshakeKind {
    Hello,
    Challenge,
    Confirm,
}
impl HandshakeKind {
    const fn tag(self) -> Tag {
        match self {
            Self::Hello => Tag::Hello,
            Self::Challenge => Tag::Challenge,
            Self::Confirm => Tag::Confirm,
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub(super) struct HandshakeFrame {
    pub kind: HandshakeKind,
    pub cookie: Cookie,
}
impl HandshakeFrame {
    pub const fn hello(nonce: Nonce) -> Self {
        Self {
            kind: HandshakeKind::Hello,
            cookie: Cookie::hello(nonce),
        }
    }
    pub const fn challenge(cookie: Cookie) -> Self {
        Self {
            kind: HandshakeKind::Challenge,
            cookie,
        }
    }
    pub const fn confirm(cookie: Cookie) -> Self {
        Self {
            kind: HandshakeKind::Confirm,
            cookie,
        }
    }
    pub fn encode(self) -> [u8; HANDSHAKE_SIZE] {
        let mut bytes = [0; HANDSHAKE_SIZE];
        bytes[..4].copy_from_slice(MAGIC);
        bytes[4] = self.kind.tag() as u8;
        bytes[5..21].copy_from_slice(self.cookie.nonce.as_bytes());
        bytes[21..29].copy_from_slice(&self.cookie.epoch.to_le_bytes());
        bytes[29..37].copy_from_slice(&self.cookie.generation.to_le_bytes());
        bytes[37..].copy_from_slice(self.cookie.mac.as_bytes());
        bytes
    }
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != HANDSHAKE_SIZE || bytes.get(..4)? != MAGIC {
            return None;
        }
        let kind = match Tag::from_repr(*bytes.get(4)?)? {
            Tag::Hello => HandshakeKind::Hello,
            Tag::Challenge => HandshakeKind::Challenge,
            Tag::Confirm => HandshakeKind::Confirm,
            Tag::Data | Tag::Keepalive | Tag::Disconnect | Tag::Accept => return None,
        };
        Some(Self {
            kind,
            cookie: Cookie {
                nonce: Nonce::from_bytes(bytes.get(5..21)?.try_into().ok()?),
                epoch: u64::from_le_bytes(bytes.get(21..29)?.try_into().ok()?),
                generation: u64::from_le_bytes(bytes.get(29..37)?.try_into().ok()?),
                mac: Session::from_bytes(bytes.get(37..)?.try_into().ok()?),
            },
        })
    }
}

pub(super) struct CookieJar {
    key: [u8; 32],
    started: Instant,
    generation: Mutex<u64>,
}

impl CookieJar {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            key: RandomBytes::generate()?,
            started: Instant::now(),
            generation: Mutex::new(1),
        })
    }

    fn epoch(&self) -> u64 {
        self.started.elapsed().as_secs() / 30
    }

    pub(super) fn epoch_is_live(&self, issued_epoch: u64) -> bool {
        let current = self.epoch();
        issued_epoch <= current && current - issued_epoch <= 1
    }

    fn sign(&self, address: SocketAddr, cookie: &Cookie) -> blake3::Hash {
        let mut hasher = blake3::Hasher::new_keyed(&self.key);
        match address {
            SocketAddr::V4(addr) => {
                hasher.update(&[4]);
                hasher.update(&addr.ip().octets());
            }
            SocketAddr::V6(addr) => {
                hasher.update(&[6]);
                hasher.update(&addr.ip().octets());
                hasher.update(&addr.scope_id().to_le_bytes());
            }
        }
        hasher.update(&address.port().to_le_bytes());
        hasher.update(cookie.nonce.as_bytes());
        hasher.update(&cookie.epoch.to_le_bytes());
        hasher.update(&cookie.generation.to_le_bytes());
        hasher.finalize()
    }

    pub fn issue(&self, address: SocketAddr, nonce: Nonce) -> Cookie {
        // Reserve the generation and observe the epoch together. Concurrent
        // accept calls must never issue an older generation with a later expiry:
        // replay watermarks rely on this ordering when they expire.
        let mut generation = self
            .generation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut cookie = Cookie {
            nonce,
            epoch: self.epoch(),
            generation: *generation,
            mac: Session::from_bytes([0; 32]),
        };
        // Exhaustion fails closed for replacements rather than wrapping backwards.
        *generation = generation.saturating_add(1);
        drop(generation);
        cookie.mac = Session::from_bytes(*self.sign(address, &cookie).as_bytes());
        cookie
    }

    pub fn verify(&self, address: SocketAddr, cookie: &Cookie) -> bool {
        self.epoch_is_live(cookie.epoch)
            && self.sign(address, cookie) == blake3::Hash::from(*cookie.mac.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_frames_match_sln2_bytes() {
        let session = Session::from_bytes([0xa5; 32]);
        let mut expected = b"SLN2\x01".to_vec();
        expected.extend_from_slice(&[0xa5; 32]);
        expected.extend_from_slice(&[0x10, 0x20]);
        assert_eq!(Frame::data(session, &[0x10, 0x20]).encode(), expected);
        assert_eq!(
            Frame::parse(&expected),
            Some(Frame::data(session, &[0x10, 0x20]))
        );
        for (control, tag) in [
            (Control::Keepalive, 2),
            (Control::Disconnect, 3),
            (Control::Accept, 7),
        ] {
            let mut expected = b"SLN2".to_vec();
            expected.push(tag);
            expected.extend_from_slice(&[0xa5; 32]);
            assert_eq!(control.encode(session).as_slice(), expected);
            assert_eq!(
                Frame::parse(&expected),
                Some(Frame {
                    session,
                    payload: Payload::Control(control)
                })
            );
            expected.push(0);
            assert!(Frame::parse(&expected).is_none());
        }
    }

    #[test]
    fn handshake_frames_match_sln2_bytes() {
        let nonce = Nonce::from_bytes([0x11; 16]);
        let cookie = Cookie {
            nonce,
            epoch: 0x0807_0605_0403_0201,
            generation: 0x1817_1615_1413_1211,
            mac: Session::from_bytes([0x22; 32]),
        };
        let mut hello = b"SLN2\x04".to_vec();
        hello.extend_from_slice(&[0x11; 16]);
        hello.extend_from_slice(&[0; 48]);
        assert_eq!(HandshakeFrame::hello(nonce).encode().as_slice(), hello);
        assert_eq!(
            HandshakeFrame::parse(&hello).unwrap().kind,
            HandshakeKind::Hello
        );
        for (frame, tag) in [
            (HandshakeFrame::challenge(cookie), 5),
            (HandshakeFrame::confirm(cookie), 6),
        ] {
            let mut expected = b"SLN2".to_vec();
            expected.push(tag);
            expected.extend_from_slice(&[0x11; 16]);
            expected.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
            expected.extend_from_slice(&[17, 18, 19, 20, 21, 22, 23, 24]);
            expected.extend_from_slice(&[0x22; 32]);
            assert_eq!(frame.encode().as_slice(), expected);
            let parsed = HandshakeFrame::parse(&expected).unwrap();
            assert_eq!(parsed.kind, frame.kind);
            assert_eq!(parsed.cookie.nonce, nonce);
            assert_eq!(parsed.cookie.epoch, cookie.epoch);
            assert_eq!(parsed.cookie.generation, cookie.generation);
            assert_eq!(parsed.cookie.mac, cookie.mac);
            assert!(HandshakeFrame::parse(&expected[..68]).is_none());
            expected.push(0);
            assert!(HandshakeFrame::parse(&expected).is_none());
        }
    }

    #[test]
    fn parsers_reject_wrong_tags_magic_and_sizes() {
        for tag in 0..=u8::MAX {
            let mut bytes = vec![0; 37];
            bytes[..4].copy_from_slice(b"SLN2");
            bytes[4] = tag;
            assert_eq!(Frame::parse(&bytes).is_some(), matches!(tag, 1 | 2 | 3 | 7));
            bytes.resize(69, 0);
            assert_eq!(
                HandshakeFrame::parse(&bytes).is_some(),
                matches!(tag, 4..=6)
            );
        }
        let mut bytes = vec![0; 65_507];
        bytes[..5].copy_from_slice(b"SLN2\x01");
        assert!(Frame::parse(&bytes).is_some());
        bytes.push(0);
        assert!(Frame::parse(&bytes).is_none());
        for len in 0..37 {
            assert!(Frame::parse(&bytes[..len]).is_none());
        }
        bytes[0] = b'X';
        assert!(Frame::parse(&bytes[..37]).is_none());
        bytes[4] = 4;
        assert!(HandshakeFrame::parse(&bytes[..69]).is_none());
    }

    #[test]
    fn hello_parsing_preserves_nonzero_cookie_fields() {
        let mut bytes = [0x33; 69];
        bytes[..5].copy_from_slice(b"SLN2\x04");
        let parsed = HandshakeFrame::parse(&bytes).unwrap();
        assert_eq!(parsed.kind, HandshakeKind::Hello);
        assert_eq!(parsed.encode(), bytes);
    }
}
