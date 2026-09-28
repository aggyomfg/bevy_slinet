use super::wire::{Control, Frame, HandshakeFrame, HandshakeKind, Payload, Session};

pub struct RawPeer;
impl RawPeer {
    pub(crate) fn respond(bytes: &[u8]) -> Option<Vec<u8>> {
        if let Some(HandshakeFrame { kind, mut cookie }) = HandshakeFrame::parse(bytes) {
            return match kind {
                HandshakeKind::Hello => {
                    cookie.mac =
                        Session::from_bytes(*blake3::hash(cookie.nonce.as_bytes()).as_bytes());
                    cookie.generation = 1;
                    Some(HandshakeFrame::challenge(cookie).encode().to_vec())
                }
                HandshakeKind::Confirm => Some(Control::Accept.encode(cookie.mac).to_vec()),
                HandshakeKind::Challenge => None,
            };
        }
        match Frame::parse(bytes) {
            Some(Frame {
                session,
                payload: Payload::Control(Control::Keepalive),
            }) => Some(Control::Keepalive.encode(session).to_vec()),
            _ => None,
        }
    }
}
