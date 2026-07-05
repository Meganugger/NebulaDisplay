// NebulaDisplay desktop viewer: a thin Tauri shell that hosts the same
// battle-tested web viewer (viewer/web) in a native window. This gives
// Windows/macOS/Linux desktop viewers a single code path: NDSP client logic
// lives in TypeScript, the shell adds a native window, fullscreen, and
// (later) UDP discovery via a Tauri command.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::net::UdpSocket;
use std::time::Duration;

/// LAN discovery bridge for the web UI (browsers cannot send UDP; the
/// desktop shell can). Returns raw JSON replies from hosts.
#[tauri::command]
fn discover_hosts() -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let Ok(socket) = UdpSocket::bind(("0.0.0.0", 0)) else {
        return out;
    };
    socket.set_broadcast(true).ok();
    socket
        .set_read_timeout(Some(Duration::from_millis(400)))
        .ok();
    if socket
        .send_to(b"NDSP-DISCOVER-1", ("255.255.255.255", 38471))
        .is_err()
    {
        return out;
    }
    let deadline = std::time::Instant::now() + Duration::from_millis(1500);
    let mut buf = [0u8; 1024];
    while std::time::Instant::now() < deadline {
        if let Ok((n, peer)) = socket.recv_from(&mut buf) {
            if let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(&buf[..n]) {
                if v.get("service").and_then(|s| s.as_str()) == Some("nebuladisplay") {
                    v["address"] = serde_json::Value::String(peer.ip().to_string());
                    out.push(v);
                }
            }
        }
    }
    out
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .invoke_handler(tauri::generate_handler![discover_hosts])
        .run(tauri::generate_context!())
        .expect("error while running NebulaDisplay viewer");
}
