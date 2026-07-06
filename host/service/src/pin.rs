//! Pairing PIN lifecycle + brute-force protection.
//!
//! * PINs are **single-use**: consumed on the first successful pairing.
//! * PINs expire after `pin_ttl_secs` and are regenerated on demand.
//! * Failed pairing attempts are counted **per source IP**; exceeding
//!   `max_pin_attempts` locks that IP out for `lockout_secs`. A fresh PIN is
//!   also generated after any failure so an attacker can't grind one PIN.

use ndsp_protocol::crypto::generate_pin;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{info, warn};

pub struct PinManager {
    digits: u32,
    ttl: Duration,
    max_attempts: u32,
    lockout: Duration,
    inner: Mutex<PinState>,
    attempts: Mutex<HashMap<IpAddr, Attempts>>,
}

struct PinState {
    pin: String,
    issued: Instant,
    used: bool,
}

struct Attempts {
    failures: u32,
    locked_until: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PinGate {
    Allowed,
    LockedOut { retry_after: Duration },
}

impl PinManager {
    pub fn new(digits: u32, ttl_secs: u64, max_attempts: u32, lockout_secs: u64) -> Self {
        Self {
            digits,
            ttl: Duration::from_secs(ttl_secs),
            max_attempts,
            lockout: Duration::from_secs(lockout_secs),
            inner: Mutex::new(PinState {
                pin: generate_pin(digits),
                issued: Instant::now(),
                used: false,
            }),
            attempts: Mutex::new(HashMap::new()),
        }
    }

    /// Current valid PIN, rotating it first if expired or already consumed.
    pub fn current_pin(&self) -> String {
        let mut s = self.inner.lock().unwrap();
        if s.used || s.issued.elapsed() > self.ttl {
            s.pin = generate_pin(self.digits);
            s.issued = Instant::now();
            s.used = false;
            info!("pairing PIN rotated");
        }
        s.pin.clone()
    }

    /// Force-rotate (panel "new PIN" button).
    pub fn rotate(&self) -> String {
        let mut s = self.inner.lock().unwrap();
        s.pin = generate_pin(self.digits);
        s.issued = Instant::now();
        s.used = false;
        s.pin.clone()
    }

    /// Gate check before starting a pairing handshake from `ip`.
    pub fn gate(&self, ip: IpAddr) -> PinGate {
        let mut map = self.attempts.lock().unwrap();
        if let Some(a) = map.get_mut(&ip) {
            if let Some(until) = a.locked_until {
                let now = Instant::now();
                if now < until {
                    return PinGate::LockedOut {
                        retry_after: until - now,
                    };
                }
                // Lockout elapsed — reset.
                a.failures = 0;
                a.locked_until = None;
            }
        }
        PinGate::Allowed
    }

    /// Record a failed PIN attempt from `ip`; rotates the PIN and possibly
    /// locks the IP out.
    pub fn record_failure(&self, ip: IpAddr) {
        {
            let mut map = self.attempts.lock().unwrap();
            let a = map.entry(ip).or_insert(Attempts {
                failures: 0,
                locked_until: None,
            });
            a.failures += 1;
            if a.failures >= self.max_attempts {
                a.locked_until = Some(Instant::now() + self.lockout);
                warn!(%ip, "pairing lockout engaged after {} failures", a.failures);
            } else {
                warn!(%ip, failures = a.failures, "pairing attempt failed");
            }
        }
        self.rotate();
    }

    /// Mark the current PIN consumed after a successful pairing and clear the
    /// IP's failure counter.
    pub fn consume(&self, ip: IpAddr) {
        self.inner.lock().unwrap().used = true;
        self.attempts.lock().unwrap().remove(&ip);
        info!(%ip, "pairing PIN consumed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip() -> IpAddr {
        "192.168.1.50".parse().unwrap()
    }

    #[test]
    fn pin_single_use() {
        let m = PinManager::new(6, 300, 5, 300);
        let p1 = m.current_pin();
        m.consume(ip());
        let p2 = m.current_pin();
        assert_ne!(p1, p2, "consumed PIN must rotate");
    }

    #[test]
    fn failures_rotate_pin_and_lock_out() {
        let m = PinManager::new(6, 300, 3, 300);
        let p1 = m.current_pin();
        m.record_failure(ip());
        assert_ne!(m.current_pin(), p1, "failure must rotate PIN");
        m.record_failure(ip());
        assert_eq!(m.gate(ip()), PinGate::Allowed);
        m.record_failure(ip());
        assert!(matches!(m.gate(ip()), PinGate::LockedOut { .. }));
        // A different IP is unaffected.
        assert_eq!(m.gate("10.0.0.9".parse().unwrap()), PinGate::Allowed);
    }
}
