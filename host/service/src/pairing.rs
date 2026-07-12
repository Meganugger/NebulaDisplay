//! Server-side authentication state machine (transport-agnostic → unit
//! testable without sockets).
//!
//! Both auth paths share the ephemeral ECDH exchange so every session gets a
//! fresh AES-256-GCM key:
//!
//! * **Pair** (first contact) — client must prove knowledge of the on-screen
//!   PIN by sealing a confirmation under the PIN-bound pairing key.
//! * **Token** (returning device) — client proves possession of its trust
//!   token via a hash bound to the full handshake transcript.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use ndsp_protocol::{
    crypto::{self, HandshakeKeys, SharedSecret},
    messages::{AuthMethod, ClientInfo, Codec, ControlMsg, ServerInfo},
    pake::{PakeSecret, PakeShare, PAKE_SUITE},
    PROTOCOL_VERSION,
};
use std::net::IpAddr;
use std::sync::Arc;
use tracing::{info, warn};

use crate::pin::PinGate;
use crate::state::AppState;

/// Result of a completed handshake.
pub struct AuthComplete {
    pub client: ClientInfo,
    pub session_key: [u8; 32],
    pub codec: Codec,
    pub input_allowed: bool,
    pub clipboard_allowed: bool,
    pub newly_paired: bool,
}

enum Phase {
    AwaitHello,
    AwaitPairStart,
    AwaitProof {
        shared: Box<SharedSecret>,
        salt: [u8; 16],
        client_pub: Vec<u8>,
        server_pub: Vec<u8>,
    },
    /// PAKE run in flight: waiting for the PIN-proving `pair_confirm`.
    AwaitPakeConfirm {
        secret: Box<PakeSecret>,
        salt: [u8; 16],
    },
    Done,
}

pub struct ServerHandshake {
    state: Arc<AppState>,
    peer_ip: IpAddr,
    phase: Phase,
    nonce: [u8; 16],
    client: Option<ClientInfo>,
    auth: Option<AuthMethod>,
    client_codecs: Vec<Codec>,
}

/// What the state machine wants the transport to do after each message.
pub struct Step {
    pub replies: Vec<ControlMsg>,
    pub complete: Option<AuthComplete>,
    /// When set, send replies then close the connection.
    pub reject: Option<String>,
}

impl Step {
    fn reply(msg: ControlMsg) -> Self {
        Self {
            replies: vec![msg],
            complete: None,
            reject: None,
        }
    }
    fn reject(msg: ControlMsg, why: impl Into<String>) -> Self {
        Self {
            replies: vec![msg],
            complete: None,
            reject: Some(why.into()),
        }
    }
}

impl ServerHandshake {
    pub fn new(state: Arc<AppState>, peer_ip: IpAddr) -> Self {
        Self {
            state,
            peer_ip,
            phase: Phase::AwaitHello,
            nonce: crypto::random_bytes(),
            client: None,
            auth: None,
            client_codecs: Vec::new(),
        }
    }

    /// Codec the server will actually stream, honoring client preference
    /// order among codecs the build supports.
    fn select_codec(&self) -> Codec {
        for c in &self.client_codecs {
            match c {
                Codec::Jpeg => return Codec::Jpeg,
                #[cfg(feature = "h264")]
                Codec::H264 => return Codec::H264,
                _ => continue,
            }
        }
        Codec::Jpeg
    }

    fn auth_ok(&self, input_allowed: bool, clipboard_allowed: bool) -> ControlMsg {
        ControlMsg::AuthOk {
            codec: self.select_codec(),
            mode: *self.state.mode.lock().unwrap(),
            input_allowed,
            clipboard_allowed,
        }
    }

    /// Shared tail of both pairing paths: verify the PIN-bound confirmation,
    /// enroll the device, seal its trust token, and complete the session.
    fn finish_pairing(
        &mut self,
        pair_key: [u8; 32],
        session_key: [u8; 32],
        sealed_b64: &str,
    ) -> Step {
        let Ok(sealed) = B64.decode(sealed_b64) else {
            return Step::reject(
                ControlMsg::PairResult {
                    ok: false,
                    sealed_token: None,
                    error: Some("bad encoding".into()),
                },
                "bad confirm b64",
            );
        };
        let mut expected = crypto::CONFIRM_CONTEXT.to_vec();
        expected.extend_from_slice(&self.nonce);
        match crypto::open(&pair_key, &sealed, b"") {
            Ok(pt) if pt == expected => {
                let client = self.client.clone().expect("hello precedes confirm");
                self.state.pins.consume(self.peer_ip);
                let token = match self.state.trust.lock().unwrap().enroll(
                    &client.device_id,
                    &client.name,
                    &client.platform,
                ) {
                    Ok(t) => t,
                    Err(e) => {
                        warn!("trust store write failed: {e:#}");
                        return Step::reject(
                            ControlMsg::PairResult {
                                ok: false,
                                sealed_token: None,
                                error: Some("host storage error".into()),
                            },
                            "trust store error",
                        );
                    }
                };
                let sealed_token = crypto::seal(&pair_key, &token, b"token");
                let auth_ok = self.auth_ok(false, false);
                let codec = self.select_codec();
                self.phase = Phase::Done;
                Step {
                    replies: vec![
                        ControlMsg::PairResult {
                            ok: true,
                            sealed_token: Some(B64.encode(sealed_token)),
                            error: None,
                        },
                        auth_ok,
                    ],
                    complete: Some(AuthComplete {
                        client,
                        session_key,
                        codec,
                        input_allowed: false,
                        clipboard_allowed: false,
                        newly_paired: true,
                    }),
                    reject: None,
                }
            }
            _ => {
                self.state.pins.record_failure(self.peer_ip);
                Step::reject(
                    ControlMsg::PairResult {
                        ok: false,
                        sealed_token: None,
                        error: Some("wrong PIN".into()),
                    },
                    "wrong PIN",
                )
            }
        }
    }

    pub fn process(&mut self, msg: ControlMsg) -> Step {
        match (&mut self.phase, msg) {
            (
                Phase::AwaitHello,
                ControlMsg::Hello {
                    protocol,
                    client,
                    auth,
                    codecs,
                },
            ) => {
                if protocol < ndsp_protocol::MIN_PROTOCOL_VERSION {
                    return Step::reject(
                        ControlMsg::AuthErr {
                            error: format!("protocol {protocol} too old"),
                        },
                        "protocol too old",
                    );
                }
                info!(device = %client.device_id, name = %client.name, platform = %client.platform, "hello");
                let pairing = matches!(auth, AuthMethod::Pair);
                self.client = Some(client);
                self.auth = Some(auth);
                self.client_codecs = codecs;
                self.phase = Phase::AwaitPairStart;
                Step::reply(ControlMsg::HelloAck {
                    protocol: PROTOCOL_VERSION.min(protocol),
                    server: ServerInfo {
                        name: self.state.cfg.name.clone(),
                        app_version: env!("CARGO_PKG_VERSION").into(),
                        fingerprint: self.state.fingerprint.clone(),
                    },
                    pairing_required: pairing,
                    connection_nonce: B64.encode(self.nonce),
                    pake: Some(PAKE_SUITE.to_string()),
                })
            }

            (Phase::AwaitPairStart, ControlMsg::PakeStart { client_pubkey }) => {
                if !matches!(self.auth, Some(AuthMethod::Pair)) {
                    return Step::reject(
                        ControlMsg::AuthErr {
                            error: "pake_start is only valid for pairing".into(),
                        },
                        "protocol misuse",
                    );
                }
                // Rate-limit before any expensive crypto.
                if let PinGate::LockedOut { retry_after } = self.state.pins.gate(self.peer_ip) {
                    return Step::reject(
                        ControlMsg::AuthErr {
                            error: format!(
                                "too many failed attempts; retry in {}s",
                                retry_after.as_secs()
                            ),
                        },
                        "pairing lockout",
                    );
                }
                let Ok(client_share) = B64.decode(&client_pubkey) else {
                    return Step::reject(
                        ControlMsg::AuthErr {
                            error: "bad PAKE share encoding".into(),
                        },
                        "bad pake b64",
                    );
                };
                let device_id = self
                    .client
                    .as_ref()
                    .map(|c| c.device_id.clone())
                    .unwrap_or_default();
                let pin = self.state.pins.current_pin();
                let share = match PakeShare::generate(
                    &pin,
                    &self.nonce,
                    &device_id,
                    &self.state.fingerprint,
                ) {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("PAKE share generation failed: {e}");
                        return Step::reject(
                            ControlMsg::AuthErr {
                                error: "internal PAKE error".into(),
                            },
                            "pake generate",
                        );
                    }
                };
                let server_share = share.public_bytes().to_vec();
                let secret =
                    match share.agree(&client_share, &self.nonce, &client_share, &server_share) {
                        Ok(s) => s,
                        Err(_) => {
                            // Malformed/off-curve share: hostile or broken
                            // client — counts as a failed attempt.
                            self.state.pins.record_failure(self.peer_ip);
                            return Step::reject(
                                ControlMsg::AuthErr {
                                    error: "invalid PAKE share".into(),
                                },
                                "bad pake share",
                            );
                        }
                    };
                let salt: [u8; 16] = crypto::random_bytes();
                let reply = ControlMsg::PakeChallenge {
                    server_pubkey: B64.encode(&server_share),
                    salt: B64.encode(salt),
                };
                self.phase = Phase::AwaitPakeConfirm {
                    secret: Box::new(secret),
                    salt,
                };
                Step::reply(reply)
            }

            (Phase::AwaitPakeConfirm { secret, salt }, ControlMsg::PairConfirm { sealed }) => {
                let pair_key = secret.pairing_key(salt.as_ref(), &self.nonce);
                let session_key = secret.session_key(salt.as_ref(), &self.nonce);
                self.finish_pairing(pair_key, session_key, &sealed)
            }

            (Phase::AwaitPairStart, ControlMsg::PairStart { client_pubkey }) => {
                // Rate-limit before any expensive crypto.
                if matches!(self.auth, Some(AuthMethod::Pair)) {
                    if let PinGate::LockedOut { retry_after } = self.state.pins.gate(self.peer_ip) {
                        return Step::reject(
                            ControlMsg::AuthErr {
                                error: format!(
                                    "too many failed attempts; retry in {}s",
                                    retry_after.as_secs()
                                ),
                            },
                            "pairing lockout",
                        );
                    }
                }
                let Ok(client_pub) = B64.decode(&client_pubkey) else {
                    return Step::reject(
                        ControlMsg::AuthErr {
                            error: "bad public key encoding".into(),
                        },
                        "bad pubkey b64",
                    );
                };
                let keys = HandshakeKeys::generate();
                let server_pub = keys.public_bytes().to_vec();
                let shared = match keys.agree(&client_pub) {
                    Ok(s) => s,
                    Err(_) => {
                        return Step::reject(
                            ControlMsg::AuthErr {
                                error: "invalid public key".into(),
                            },
                            "bad pubkey",
                        )
                    }
                };
                let salt: [u8; 16] = crypto::random_bytes();
                let reply = ControlMsg::PairChallenge {
                    server_pubkey: B64.encode(&server_pub),
                    salt: B64.encode(salt),
                };
                self.phase = Phase::AwaitProof {
                    shared: Box::new(shared),
                    salt,
                    client_pub,
                    server_pub,
                };
                Step::reply(reply)
            }

            (Phase::AwaitProof { shared, salt, .. }, ControlMsg::PairConfirm { sealed })
                if matches!(self.auth, Some(AuthMethod::Pair)) =>
            {
                // Legacy PIN-HKDF pairing (pre-PAKE clients). Kept for the
                // mobile viewers until they ship the PAKE path; hosts can
                // refuse it entirely via `allow_legacy_pairing = false`.
                if !self.state.cfg.file.allow_legacy_pairing {
                    return Step::reject(
                        ControlMsg::PairResult {
                            ok: false,
                            sealed_token: None,
                            error: Some(
                                "this host requires PAKE pairing — update your viewer app".into(),
                            ),
                        },
                        "legacy pairing disabled",
                    );
                }
                let pin = self.state.pins.current_pin();
                let pair_key = shared.pairing_key(salt.as_ref(), &pin, &self.nonce);
                let session_key = shared.session_key(salt.as_ref(), &self.nonce);
                self.finish_pairing(pair_key, session_key, &sealed)
            }

            (
                Phase::AwaitProof {
                    shared,
                    salt,
                    client_pub,
                    server_pub,
                },
                ControlMsg::TokenProof { proof },
            ) => {
                let Some(AuthMethod::Token { device_id }) = self.auth.clone() else {
                    return Step::reject(
                        ControlMsg::AuthErr {
                            error: "token proof without token auth".into(),
                        },
                        "protocol misuse",
                    );
                };
                let Ok(proof) = B64.decode(&proof) else {
                    return Step::reject(
                        ControlMsg::AuthErr {
                            error: "bad proof encoding".into(),
                        },
                        "bad proof b64",
                    );
                };
                let verified = self.state.trust.lock().unwrap().verify(
                    &device_id,
                    &self.nonce,
                    client_pub,
                    server_pub,
                    &proof,
                );
                match verified {
                    Some(dev) => {
                        let client = self.client.clone().expect("hello precedes proof");
                        let session_key = shared.session_key(salt.as_ref(), &self.nonce);
                        let input_allowed = dev.input_allowed;
                        let clipboard_allowed = dev.clipboard_allowed;
                        let auth_ok = self.auth_ok(input_allowed, clipboard_allowed);
                        let codec = self.select_codec();
                        info!(device = %device_id, "token reconnect ok");
                        self.phase = Phase::Done;
                        Step {
                            replies: vec![auth_ok],
                            complete: Some(AuthComplete {
                                client,
                                session_key,
                                codec,
                                input_allowed,
                                clipboard_allowed,
                                newly_paired: false,
                            }),
                            reject: None,
                        }
                    }
                    None => {
                        warn!(device = %device_id, ip = %self.peer_ip, "token proof rejected");
                        Step::reject(
                            ControlMsg::AuthErr {
                                error: "unknown device or bad proof; pair again".into(),
                            },
                            "token rejected",
                        )
                    }
                }
            }

            (_, other) => Step::reject(
                ControlMsg::AuthErr {
                    error: format!(
                        "unexpected message {:?} during handshake",
                        variant_name(&other)
                    ),
                },
                "protocol violation",
            ),
        }
    }
}

fn variant_name(msg: &ControlMsg) -> &'static str {
    match msg {
        ControlMsg::Hello { .. } => "hello",
        ControlMsg::HelloAck { .. } => "hello_ack",
        ControlMsg::PairStart { .. } => "pair_start",
        ControlMsg::PairChallenge { .. } => "pair_challenge",
        ControlMsg::PakeStart { .. } => "pake_start",
        ControlMsg::PakeChallenge { .. } => "pake_challenge",
        ControlMsg::PairConfirm { .. } => "pair_confirm",
        ControlMsg::PairResult { .. } => "pair_result",
        ControlMsg::TokenProof { .. } => "token_proof",
        ControlMsg::AuthOk { .. } => "auth_ok",
        ControlMsg::AuthErr { .. } => "auth_err",
        _ => "post-auth message",
    }
}
