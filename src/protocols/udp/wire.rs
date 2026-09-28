//! Versioned UDP frames and stateless, address-bound handshake cookies.
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::time::Instant;

pub(super) const MAGIC: &[u8; 4] = b"SLN2";
pub(super) const DATA: u8 = 1;
pub(super) const KEEPALIVE: u8 = 2;
pub(super) const DISCONNECT: u8 = 3;
pub(super) const HELLO: u8 = 4;
pub(super) const CHALLENGE: u8 = 5;
pub(super) const CONFIRM: u8 = 6;
pub(super) const ACCEPT: u8 = 7;
pub(super) const HEADER: usize = 5 + 32;
pub(super) const HANDSHAKE_SIZE: usize = 5 + 16 + 8 + 8 + 32;
pub(super) type Session = [u8; 32];

pub(super) fn random<const N: usize>() -> io::Result<[u8; N]> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(|err| io::Error::other(err.to_string()))?;
    Ok(bytes)
}

pub(super) fn control(tag: u8, session: &Session) -> [u8; HEADER] {
    let mut bytes = [0; HEADER];
    bytes[..4].copy_from_slice(MAGIC);
    bytes[4] = tag;
    bytes[5..].copy_from_slice(session);
    bytes
}

pub(super) fn frame(tag: u8, session: &Session, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER + payload.len());
    bytes.extend_from_slice(&control(tag, session));
    bytes.extend_from_slice(payload);
    bytes
}

pub(super) fn parse_frame(bytes: &[u8]) -> Option<(u8, Session, &[u8])> {
    if bytes.len() < HEADER || bytes.len() > super::MAX_DATAGRAM_SIZE || &bytes[..4] != MAGIC {
        return None;
    }
    let tag = bytes[4];
    if tag != DATA && (!matches!(tag, KEEPALIVE | DISCONNECT | ACCEPT) || bytes.len() != HEADER) {
        return None;
    }
    Some((tag, bytes[5..HEADER].try_into().ok()?, &bytes[HEADER..]))
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Cookie {
    pub nonce: [u8; 16],
    pub epoch: u64,
    pub generation: u64,
    pub mac: Session,
}

impl Cookie {
    pub fn hello(nonce: [u8; 16]) -> Self {
        Self {
            nonce,
            epoch: 0,
            generation: 0,
            mac: [0; 32],
        }
    }

    pub fn encode(&self, tag: u8) -> [u8; HANDSHAKE_SIZE] {
        let mut bytes = [0; HANDSHAKE_SIZE];
        bytes[..4].copy_from_slice(MAGIC);
        bytes[4] = tag;
        bytes[5..21].copy_from_slice(&self.nonce);
        bytes[21..29].copy_from_slice(&self.epoch.to_le_bytes());
        bytes[29..37].copy_from_slice(&self.generation.to_le_bytes());
        bytes[37..].copy_from_slice(&self.mac);
        bytes
    }

    pub fn parse(bytes: &[u8]) -> Option<(u8, Self)> {
        if bytes.len() != HANDSHAKE_SIZE
            || &bytes[..4] != MAGIC
            || !matches!(bytes[4], HELLO | CHALLENGE | CONFIRM)
        {
            return None;
        }
        Some((
            bytes[4],
            Self {
                nonce: bytes[5..21].try_into().ok()?,
                epoch: u64::from_le_bytes(bytes[21..29].try_into().ok()?),
                generation: u64::from_le_bytes(bytes[29..37].try_into().ok()?),
                mac: bytes[37..].try_into().ok()?,
            },
        ))
    }
}

pub(super) struct CookieJar {
    key: [u8; 32],
    started: Instant,
    generation: AtomicU64,
}

impl CookieJar {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            key: random()?,
            started: Instant::now(),
            generation: AtomicU64::new(1),
        })
    }

    fn epoch(&self) -> u64 {
        self.started.elapsed().as_secs() / 30
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
        hasher.update(&cookie.nonce);
        hasher.update(&cookie.epoch.to_le_bytes());
        hasher.update(&cookie.generation.to_le_bytes());
        hasher.finalize()
    }

    pub fn issue(&self, address: SocketAddr, nonce: [u8; 16]) -> Cookie {
        let mut cookie = Cookie {
            nonce,
            epoch: self.epoch(),
            generation: self.generation.fetch_add(1, Ordering::Relaxed),
            mac: [0; 32],
        };
        cookie.mac = *self.sign(address, &cookie).as_bytes();
        cookie
    }

    pub fn verify(&self, address: SocketAddr, cookie: &Cookie) -> bool {
        let epoch = self.epoch();
        cookie.epoch <= epoch
            && epoch - cookie.epoch <= 1
            && self.sign(address, cookie) == blake3::Hash::from(cookie.mac)
    }
}
