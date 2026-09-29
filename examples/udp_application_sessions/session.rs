//! Application protocol only: identifiers are not authentication credentials.
use bevy::platform::time::Instant;
use bitcode::{Decode, Encode};
use std::{collections::HashMap, time::Duration};

#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub enum Body {
    Hello,
    Welcome,
    Data(Vec<u8>),
    Ping,
    Close,
}
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct Packet {
    pub channel: u8,
    pub generation: u64,
    pub id: Vec<u8>,
    pub body: Body,
}
struct Entry {
    generation: u64,
    id: Vec<u8>,
    last_received: Instant,
    active: bool,
}
/// Application replay records are retained for the lifetime of this demo.
#[derive(Default)]
pub struct Sessions {
    entries: HashMap<u8, Entry>,
}
impl Sessions {
    pub fn handle(&mut self, packet: &Packet, received_at: Instant) -> Option<Packet> {
        if packet.id.is_empty() || packet.id.len() > 128 {
            return None;
        }
        if packet.body == Body::Hello {
            match self.entries.get_mut(&packet.channel) {
                Some(entry) if packet.generation < entry.generation => return None,
                Some(entry) if packet.generation == entry.generation => {
                    if entry.id != packet.id || !entry.active {
                        return None;
                    }
                    entry.last_received = entry.last_received.max(received_at);
                }
                _ => {
                    if !self.entries.contains_key(&packet.channel) && self.entries.len() >= 8 {
                        return None;
                    }
                    self.entries.insert(
                        packet.channel,
                        Entry {
                            generation: packet.generation,
                            id: packet.id.clone(),
                            last_received: received_at,
                            active: true,
                        },
                    );
                }
            }
            return Some(Packet {
                body: Body::Welcome,
                ..packet.clone()
            });
        }
        let entry = self.entries.get_mut(&packet.channel)?;
        if entry.generation != packet.generation || entry.id != packet.id {
            return None;
        }
        // Repeat Close acknowledgement after loss, without resurrecting the session.
        if !entry.active {
            return (packet.body == Body::Close).then(|| packet.clone());
        }
        match packet.body {
            Body::Data(_) | Body::Ping => {
                entry.last_received = entry.last_received.max(received_at);
            }
            Body::Close => {
                entry.active = false;
            }
            Body::Hello | Body::Welcome => return None,
        }
        Some(packet.clone())
    }
    pub fn expire(&mut self, now: Instant, timeout: Duration) {
        for entry in self.entries.values_mut() {
            if now.saturating_duration_since(entry.last_received) >= timeout {
                entry.active = false;
            }
        }
    }
    pub fn has_history(&self) -> bool {
        !self.entries.is_empty()
    }
    pub fn active(&self) -> usize {
        self.entries.values().filter(|entry| entry.active).count()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    fn packet(channel: u8, generation: u64, size: usize, body: Body) -> Packet {
        Packet {
            channel,
            generation,
            id: vec![channel; size],
            body,
        }
    }
    #[test]
    fn arbitrary_ids_and_two_sessions_share_one_peer() {
        let now = Instant::now();
        for size in [8, 48, 64] {
            let mut sessions = Sessions::default();
            for channel in [1, 2] {
                let mut hello = packet(channel, 1, size, Body::Hello);
                assert_eq!(sessions.handle(&hello, now).unwrap().body, Body::Welcome);
                hello.body = Body::Data(vec![7]);
                assert_eq!(sessions.handle(&hello, now).unwrap(), hello);
            }
            assert_eq!(sessions.active(), 2);
            sessions.handle(&packet(1, 1, size, Body::Close), now);
            assert_eq!(sessions.active(), 1);
        }
    }
    #[test]
    fn old_packets_and_requests_cannot_replace_or_close_new_session() {
        let now = Instant::now();
        let mut sessions = Sessions::default();
        let old = packet(1, 1, 48, Body::Hello);
        let new = packet(1, 2, 64, Body::Hello);
        assert!(sessions.handle(&old, now).is_some());
        assert!(sessions.handle(&new, now).is_some());
        for body in [Body::Hello, Body::Data(vec![1]), Body::Ping, Body::Close] {
            assert!(sessions
                .handle(
                    &Packet {
                        body,
                        ..old.clone()
                    },
                    now
                )
                .is_none());
        }
        assert_eq!(sessions.active(), 1);
        assert!(sessions
            .handle(
                &Packet {
                    body: Body::Close,
                    ..new.clone()
                },
                now
            )
            .is_some());
        assert!(sessions.handle(&new, now).is_none());
    }
    #[test]
    fn repeated_hello_keeps_handshake_alive_without_reviving_expired_sessions() {
        let now = Instant::now();
        let mut sessions = Sessions::default();
        let hello = packet(1, 1, 48, Body::Hello);
        let timeout = Duration::from_millis(500);
        for millis in (0..=450).step_by(50) {
            assert_eq!(
                sessions
                    .handle(&hello, now + Duration::from_millis(millis))
                    .unwrap()
                    .body,
                Body::Welcome
            );
        }
        // A delayed event must not move the last-received timestamp backwards.
        sessions.handle(&hello, now);
        sessions.expire(now + timeout, timeout);
        assert!(sessions
            .handle(&hello, now + Duration::from_millis(550))
            .is_some());
        // A different identifier must not refresh the active generation.
        let mut invalid = hello.clone();
        invalid.id = vec![9; 48];
        assert!(sessions
            .handle(&invalid, now + Duration::from_millis(1000))
            .is_none());
        sessions.expire(now + Duration::from_millis(1050), timeout);
        assert_eq!(sessions.active(), 0);
        assert!(sessions
            .handle(&hello, now + Duration::from_millis(1100))
            .is_none());
    }
    #[test]
    fn only_valid_fresh_packets_refresh_liveness() {
        let now = Instant::now();
        let mut sessions = Sessions::default();
        let hello = packet(1, 1, 48, Body::Hello);
        sessions.handle(&hello, now);
        sessions.handle(
            &Packet {
                body: Body::Ping,
                ..hello.clone()
            },
            now + Duration::from_secs(2),
        );
        sessions.handle(
            &Packet {
                body: Body::Ping,
                ..hello.clone()
            },
            now,
        );
        sessions.expire(now + Duration::from_secs(3), Duration::from_secs(2));
        assert_eq!(sessions.active(), 1);
        sessions.handle(
            &Packet {
                id: vec![9; 48],
                body: Body::Ping,
                ..hello
            },
            now + Duration::from_secs(4),
        );
        sessions.expire(now + Duration::from_secs(4), Duration::from_secs(2));
        assert_eq!(sessions.active(), 0);
    }
}
