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

/// Bounds real socket scenarios even while Tokio's clock is paused.
pub async fn wall_timeout<F: std::future::Future>(future: F) -> F::Output {
    let (finished, completion) = std::sync::mpsc::channel::<()>();
    let (expired, deadline) = tokio::sync::oneshot::channel();
    let _watchdog = std::thread::spawn(move || {
        if matches!(
            completion.recv_timeout(std::time::Duration::from_secs(5)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ) {
            let _ = expired.send(());
        }
    });
    let result = tokio::select! {
        result = future => result,
        _ = deadline => panic!("UDP socket test exceeded its five-second wall-clock budget"),
    };
    drop(finished);
    result
}
