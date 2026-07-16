//! SPAKE2 interoperability test server (stdio JSON lines).
//!
//! Drives the *real* [`ndsp_protocol::spake2::Spake2Server`] against a
//! foreign client implementation (the Android/Kotlin one lives in
//! `viewer/android/interop/`). **Test tool only** — it prints derived keys
//! so the harness can assert both stacks agree byte-for-byte; never reuse
//! it outside tests.
//!
//! Protocol (one JSON object per line):
//!   in : {"pin": "123456", "nonce": "<b64>", "pa": "<b64>"}
//!   out: {"pb": "<b64>"}                      (or {"error": "..."} and exit)
//!   in : {"mac": "<b64>"}
//!   out: {"ok": bool, "mac": "<b64 server confirm>",
//!         "session_key": "<b64>", "token_key": "<b64>"}

use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use ndsp_protocol::spake2::{mac_equal, Spake2Server};
use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut out = std::io::stdout();

    let first: serde_json::Value =
        serde_json::from_str(&lines.next().expect("first line").expect("read")).expect("json");
    let pin = first["pin"].as_str().expect("pin");
    let nonce = B64
        .decode(first["nonce"].as_str().expect("nonce"))
        .expect("nonce b64");
    let pa = B64
        .decode(first["pa"].as_str().expect("pa"))
        .expect("pa b64");

    let server = match Spake2Server::respond(pin, &nonce, &pa) {
        Ok(s) => s,
        Err(e) => {
            writeln!(out, "{}", serde_json::json!({ "error": e.to_string() })).unwrap();
            std::process::exit(1);
        }
    };
    writeln!(
        out,
        "{}",
        serde_json::json!({ "pb": B64.encode(server.share()) })
    )
    .unwrap();
    out.flush().unwrap();

    let second: serde_json::Value =
        serde_json::from_str(&lines.next().expect("second line").expect("read")).expect("json");
    let mac = B64
        .decode(second["mac"].as_str().expect("mac"))
        .expect("mac b64");
    let keys = server.into_keys();
    let ok = mac_equal(&mac, &keys.confirm_client);
    writeln!(
        out,
        "{}",
        serde_json::json!({
            "ok": ok,
            "mac": B64.encode(keys.confirm_server),
            "session_key": B64.encode(keys.session_key),
            "token_key": B64.encode(keys.token_key),
        })
    )
    .unwrap();
}
