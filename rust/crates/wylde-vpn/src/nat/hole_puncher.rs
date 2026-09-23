//! Coordinated UDP hole-punching. Port of `Wylde/VPN/nat/hole_puncher.py`.
//!
//! Both peers fire empty UDP datagrams at each other's STUN-mapped
//! endpoint. The outbound packets create NAT state on each side; the
//! WireGuard handshake can flow once both NATs have a hole open. The
//! coordination signal goes through the pairing/register exchange
//! (mobile sends its mapped endpoint in `/api/link/register`; the
//! desktop emits its endpoint in the registration response).
//!
//! The single-sided variant at [`super::stun::punch_hole`] is kept
//! colocated with the STUN code because the discovery path does
//! `classify → punch_hole` back-to-back. This module's [`punch`] is
//! the richer two-sided variant.

use std::net::UdpSocket;
use std::sync::Arc;
use std::time::Duration;

/// Send a burst of empty UDP datagrams to `remote_endpoint` from
/// `local_port`. Returns `true` if every datagram was sent successfully
/// (this does NOT confirm receipt — that's WireGuard's job after the
/// hole is open).
///
/// Defaults mirror Python: 8 attempts × 0.25s interval.
pub fn punch(remote_endpoint: &str, local_port: u16, attempts: u32, interval: Duration) -> bool {
    punch_with(&RealSocket, remote_endpoint, local_port, attempts, interval)
}

/// Socket factory abstraction. Tests inject a counting/recording mock.
pub trait Socket: Send + Sync {
    fn bind(&self, local_port: u16) -> Option<Arc<dyn SendSocket>>;
}

/// Per-send abstraction. The `Arc` indirection in [`Socket::bind`]'s
/// return type lets the mock keep its recording state alive past the
/// trait-object boundary without `unsafe`.
pub trait SendSocket: Send + Sync {
    fn send(&self, target: &str, payload: &[u8]) -> bool;
}

struct RealSocket;
impl Socket for RealSocket {
    fn bind(&self, local_port: u16) -> Option<Arc<dyn SendSocket>> {
        let sock = UdpSocket::bind(("0.0.0.0", local_port)).ok()?;
        Some(Arc::new(RealSendSocket { sock }))
    }
}

struct RealSendSocket {
    sock: UdpSocket,
}

impl SendSocket for RealSendSocket {
    fn send(&self, target: &str, payload: &[u8]) -> bool {
        self.sock.send_to(payload, target).is_ok()
    }
}

pub fn punch_with(
    socket: &dyn Socket,
    remote_endpoint: &str,
    local_port: u16,
    attempts: u32,
    interval: Duration,
) -> bool {
    let Some((host, port)) = remote_endpoint.rsplit_once(':') else {
        return false;
    };
    let port: u16 = match port.parse() {
        Ok(p) => p,
        Err(_) => return false,
    };
    let Some(sock) = socket.bind(local_port) else {
        tracing::warn!("hole_puncher: cannot bind local port {local_port}");
        return false;
    };

    let target = format!("{host}:{port}");
    let mut sent = 0u32;
    for _ in 0..attempts {
        if sock.send(&target, b"\x00") {
            sent += 1;
        }
        std::thread::sleep(interval);
    }
    tracing::info!("hole_puncher: sent {sent}/{attempts} datagrams to {remote_endpoint}");
    sent == attempts
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockState {
        binds: Vec<u16>,
        sends: Vec<(String, Vec<u8>)>,
        send_count: u32,
    }

    struct MockSocket {
        state: Arc<Mutex<MockState>>,
        fail_bind: bool,
        fail_send_after: Option<u32>,
    }

    impl MockSocket {
        fn new() -> Self {
            Self {
                state: Arc::new(Mutex::new(MockState::default())),
                fail_bind: false,
                fail_send_after: None,
            }
        }
    }

    struct MockSendSocket {
        state: Arc<Mutex<MockState>>,
        fail_after: Option<u32>,
    }

    impl Socket for MockSocket {
        fn bind(&self, local_port: u16) -> Option<Arc<dyn SendSocket>> {
            self.state.lock().unwrap().binds.push(local_port);
            if self.fail_bind {
                None
            } else {
                Some(Arc::new(MockSendSocket {
                    state: Arc::clone(&self.state),
                    fail_after: self.fail_send_after,
                }))
            }
        }
    }

    impl SendSocket for MockSendSocket {
        fn send(&self, target: &str, payload: &[u8]) -> bool {
            let mut s = self.state.lock().unwrap();
            s.send_count += 1;
            if let Some(limit) = self.fail_after {
                if s.send_count > limit {
                    return false;
                }
            }
            s.sends.push((target.to_string(), payload.to_vec()));
            true
        }
    }

    #[test]
    fn punch_dispatches_every_datagram() {
        let mock = MockSocket::new();
        let ok = punch_with(
            &mock,
            "192.168.1.10:5000",
            51820,
            3,
            Duration::from_millis(0),
        );
        assert!(ok);
        let s = mock.state.lock().unwrap();
        assert_eq!(s.sends.len(), 3);
        for (target, payload) in s.sends.iter() {
            assert_eq!(target, "192.168.1.10:5000");
            assert_eq!(payload, &b"\x00".to_vec());
        }
        assert_eq!(s.binds.as_slice(), &[51820]);
    }

    #[test]
    fn punch_returns_false_on_unparseable_endpoint() {
        let mock = MockSocket::new();
        assert!(!punch_with(&mock, "bogus", 1, 5, Duration::from_millis(0)));
        assert!(mock.state.lock().unwrap().binds.is_empty());
    }

    #[test]
    fn punch_returns_false_on_nonnumeric_port() {
        let mock = MockSocket::new();
        assert!(!punch_with(
            &mock,
            "host:notanum",
            1,
            5,
            Duration::from_millis(0)
        ));
    }

    #[test]
    fn punch_returns_false_when_bind_fails() {
        let mut mock = MockSocket::new();
        mock.fail_bind = true;
        assert!(!punch_with(
            &mock,
            "1.2.3.4:99",
            123,
            5,
            Duration::from_millis(0)
        ));
        assert_eq!(mock.state.lock().unwrap().binds.as_slice(), &[123]);
    }

    #[test]
    fn punch_returns_false_if_any_send_fails() {
        let mut mock = MockSocket::new();
        mock.fail_send_after = Some(2);
        let ok = punch_with(&mock, "1.2.3.4:99", 123, 5, Duration::from_millis(0));
        assert!(!ok);
        assert_eq!(mock.state.lock().unwrap().sends.len(), 2);
    }
}
