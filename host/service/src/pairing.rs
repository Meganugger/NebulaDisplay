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
    spake2::{mac_equal, Spake2Server},
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
    pub audio_allowed: bool,
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
    AwaitSpake2Confirm {
        server: Box<Spake2Server>,
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
    /// order among codecs this host can encode (build features + hardware).
    fn select_codec(&self) -> Codec {
        self.client_codecs
            .iter()
            .copied()
            .find(|c| crate::encode::supports(*c))
            .unwrap_or(Codec::Jpeg)
    }

    fn auth_ok(&self, input_allowed: bool) -> ControlMsg {
        ControlMsg::AuthOk {
            codec: self.select_codec(),
            mode: *self.state.mode.lock().unwrap(),
            input_allowed,
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
                if matches!(auth, AuthMethod::Pair) && !self.state.cfg.file.allow_legacy_pairing {
                    return Step::reject(
                        ControlMsg::AuthErr {
                            error: "this host requires SPAKE2 pairing (legacy PIN pairing is \
                                    disabled) — update your viewer app"
                                .into(),
                        },
                        "legacy pairing disabled",
                    );
                }
                let pairing = matches!(auth, AuthMethod::Pair | AuthMethod::PairSpake2);
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
                })
            }

            (Phase::AwaitPairStart, ControlMsg::Spake2Start { share })
                if matches!(self.auth, Some(AuthMethod::PairSpake2)) =>
            {
                // Rate-limit before any curve arithmetic.
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
                let Ok(client_share) = B64.decode(&share) else {
                    return Step::reject(
                        ControlMsg::AuthErr {
                            error: "bad share encoding".into(),
                        },
                        "bad spake2 share b64",
                    );
                };
                let pin = self.state.pins.current_pin();
                let server = match Spake2Server::respond(&pin, &self.nonce, &client_share) {
                    Ok(s) => s,
                    Err(_) => {
                        // Malformed share = active protocol abuse, not a PIN
                        // guess — still counts against the attempt budget.
                        self.state.pins.record_failure(self.peer_ip);
                        return Step::reject(
                            ControlMsg::AuthErr {
                                error: "invalid SPAKE2 share".into(),
                            },
                            "bad spake2 share",
                        );
                    }
                };
                let reply = ControlMsg::Spake2Challenge {
                    share: B64.encode(server.share()),
                };
                self.phase = Phase::AwaitSpake2Confirm {
                    server: Box::new(server),
                };
                Step::reply(reply)
            }

            (Phase::AwaitSpake2Confirm { server }, ControlMsg::Spake2Confirm { mac }) => {
                let Ok(mac) = B64.decode(&mac) else {
                    return Step::reject(
                        ControlMsg::Spake2Result {
                            ok: false,
                            mac: None,
                            sealed_token: None,
                            error: Some("bad encoding".into()),
                        },
                        "bad spake2 mac b64",
                    );
                };
                if !mac_equal(&mac, &server.keys().confirm_client) {
                    self.state.pins.record_failure(self.peer_ip);
                    return Step::reject(
                        ControlMsg::Spake2Result {
                            ok: false,
                            mac: None,
                            sealed_token: None,
                            error: Some("wrong PIN".into()),
                        },
                        "wrong PIN (spake2)",
                    );
                }
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
                            ControlMsg::Spake2Result {
                                ok: false,
                                mac: None,
                                sealed_token: None,
                                error: Some("host storage error".into()),
                            },
                            "trust store error",
                        );
                    }
                };
                let keys = match std::mem::replace(&mut self.phase, Phase::Done) {
                    Phase::AwaitSpake2Confirm { server } => server.into_keys(),
                    _ => unreachable!("phase checked by match arm"),
                };
                let sealed_token = crypto::seal(&keys.token_key, &token, b"token");
                let auth_ok = self.auth_ok(false);
                let codec = self.select_codec();
                Step {
                    replies: vec![
                        ControlMsg::Spake2Result {
                            ok: true,
                            mac: Some(B64.encode(keys.confirm_server)),
                            sealed_token: Some(B64.encode(sealed_token)),
                            error: None,
                        },
                        auth_ok,
                    ],
                    complete: Some(AuthComplete {
                        client,
                        session_key: keys.session_key,
                        codec,
                        input_allowed: false,
                        clipboard_allowed: false,
                        audio_allowed: true,
                        newly_paired: true,
                    }),
                    reject: None,
                }
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
                let Ok(sealed) = B64.decode(&sealed) else {
                    return Step::reject(
                        ControlMsg::PairResult {
                            ok: false,
                            sealed_token: None,
                            error: Some("bad encoding".into()),
                        },
                        "bad confirm b64",
                    );
                };
                let pin = self.state.pins.current_pin();
                let pair_key = shared.pairing_key(salt.as_ref(), &pin, &self.nonce);
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
                        let session_key = shared.session_key(salt.as_ref(), &self.nonce);
                        let auth_ok = self.auth_ok(false);
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
                                audio_allowed: true,
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
                        let auth_ok = self.auth_ok(input_allowed);
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
                                clipboard_allowed: dev.clipboard_allowed,
                                audio_allowed: dev.audio_allowed,
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
        ControlMsg::PairConfirm { .. } => "pair_confirm",
        ControlMsg::PairResult { .. } => "pair_result",
        ControlMsg::Spake2Start { .. } => "spake2_start",
        ControlMsg::Spake2Challenge { .. } => "spake2_challenge",
        ControlMsg::Spake2Confirm { .. } => "spake2_confirm",
        ControlMsg::Spake2Result { .. } => "spake2_result",
        ControlMsg::TokenProof { .. } => "token_proof",
        ControlMsg::AuthOk { .. } => "auth_ok",
        ControlMsg::AuthErr { .. } => "auth_err",
        _ => "post-auth message",
    }
}
