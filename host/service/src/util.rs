//! Small shared helpers.

use std::net::IpAddr;
use std::time::{SystemTime, UNIX_EPOCH};

/// Microseconds since the unix epoch (host clock; used for frame timestamps
/// and Ping/Pong clock sync).
pub fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_micros() as u64
}

/// Parse "1280x720" style sizes.
pub fn parse_size(s: &str) -> anyhow::Result<(u32, u32)> {
    let (w, h) = s
        .split_once(['x', 'X'])
        .ok_or_else(|| anyhow::anyhow!("expected WIDTHxHEIGHT, got {s:?}"))?;
    let w: u32 = w.trim().parse()?;
    let h: u32 = h.trim().parse()?;
    anyhow::ensure!(
        (16..=7680).contains(&w) && (16..=4320).contains(&h),
        "size out of range"
    );
    Ok((w, h))
}

/// Best-effort list of non-loopback local IPv4 addresses (for the banner and
/// QR links). Uses the "connect a UDP socket" trick plus interface probing.
pub fn local_ips() -> Vec<IpAddr> {
    let mut out = Vec::new();
    // Primary route trick: no packets are sent for UDP connect.
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("192.0.2.1:9").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                if !addr.ip().is_loopback() && !addr.ip().is_unspecified() {
                    out.push(addr.ip());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_size_ok() {
        assert_eq!(parse_size("1280x720").unwrap(), (1280, 720));
        assert_eq!(parse_size("3840X2160").unwrap(), (3840, 2160));
    }

    #[test]
    fn parse_size_rejects_garbage() {
        assert!(parse_size("huge").is_err());
        assert!(parse_size("1x1").is_err());
        assert!(parse_size("100000x100").is_err());
    }
}
