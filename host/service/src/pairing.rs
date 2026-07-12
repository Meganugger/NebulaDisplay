//! Server-side authentication state machine (transport-agnostic → unit
//! testable without sockets).
//!
//! Every auth path yields a fresh AES-256-GCM session key:
//!
//! * **PairSpake2** (first contact, preferred) — SPAKE2 PAKE over the PIN;
//!   passive transcripts cannot be PIN-ground offline (`ndsp_protocol::pake`).
//! * **Pair** (first contact, legacy) — ephemeral ECDH + PIN-bound HKDF;
//!   kept for older clients, can be disabled via `allow_legacy_pair = false`.
//! * **Token** (returning device) — ephemeral ECDH; client proves possession
//!   of its trust token via a hash bound to the full handshake transcript.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use ndsp_protocol::{
    crypto::{self, HandshakeKeys, SharedSecret},
    messages::{AuthMethod, ClientInfo, Codec, ControlMsg, ServerInfo},
    pake::{PakeKeys, Spake2Server},
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
    AwaitProof(ProofSecret),
    Done,
}

/// Key material carried between `PairStart`/`PairChallenge` and the proof
/// message, per auth method.
enum ProofSecret {
    /// Legacy pairing + token reconnect (ephemeral ECDH).
    Ecdh {
        shared: Box<SharedSecret>,
        salt: [u8; 16],
        client_pub: Vec<u8>,
        server_pub: Vec<u8>,
    },
    /// SPAKE2 pairing: the exchange completes at `PairStart` server-side;
    /// only the derived keys are kept.
    Spake2 { keys: Box<PakeKeys> },
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
                if matches!(auth, AuthMethod::Pair) && !self.state.cfg.file.allow_legacy_pair {
                    return Step::reject(
                        ControlMsg::AuthErr {
                            error: "legacy PIN pairing is disabled on this host; \
                                    update the viewer (SPAKE2 pairing required)"
                                .into(),
                        },
                        "legacy pairing disabled",
                    );
                }
                info!(device = %client.device_id, name = %client.name, platform = %client.platform, "hello");
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

            (Phase::AwaitPairStart, ControlMsg::PairStart { client_pubkey }) => {
                // Rate-limit before any expensive crypto.
                if matches!(
                    self.auth,
                    Some(AuthMethod::Pair) | Some(AuthMethod::PairSpake2)
                ) {
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

                if matches!(self.auth, Some(AuthMethod::PairSpake2)) {
                    // SPAKE2: pA arrives; produce pB + salt, derive keys now.
                    let pin = self.state.pins.current_pin();
                    let server = Spake2Server::start(&pin, &self.nonce);
                    let server_pub = server.public_bytes().to_vec();
                    let salt: [u8; 16] = crypto::random_bytes();
                    let keys = match server.finish(&client_pub, &self.nonce, &salt) {
                        Ok(k) => k,
                        Err(_) => {
                            return Step::reject(
                                ControlMsg::AuthErr {
                                    error: "invalid SPAKE2 element".into(),
                                },
                                "bad spake2 element",
                            )
                        }
                    };
                    let reply = ControlMsg::PairChallenge {
                        server_pubkey: B64.encode(&server_pub),
                        salt: B64.encode(salt),
                    };
                    self.phase = Phase::AwaitProof(ProofSecret::Spake2 {
                        keys: Box::new(keys),
                    });
                    return Step::reply(reply);
                }

                // Legacy pairing / token reconnect: ephemeral ECDH.
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
                self.phase = Phase::AwaitProof(ProofSecret::Ecdh {
                    shared: Box::new(shared),
                    salt,
                    client_pub,
                    server_pub,
                });
                Step::reply(reply)
            }

            (Phase::AwaitProof(secret), ControlMsg::PairConfirm { sealed })
                if matches!(
                    self.auth,
                    Some(AuthMethod::Pair) | Some(AuthMethod::PairSpake2)
                ) =>
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
                // Both methods land here; only the key derivation differs.
                let (pair_key, session_key) = match secret {
                    ProofSecret::Ecdh { shared, salt, .. } => {
                        let pin = self.state.pins.current_pin();
                        (
                            shared.pairing_key(salt.as_ref(), &pin, &self.nonce),
                            shared.session_key(salt.as_ref(), &self.nonce),
                        )
                    }
                    ProofSecret::Spake2 { keys } => (keys.pairing_key, keys.session_key),
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

            (Phase::AwaitProof(secret), ControlMsg::TokenProof { proof }) => {
                let Some(AuthMethod::Token { device_id }) = self.auth.clone() else {
                    return Step::reject(
                        ControlMsg::AuthErr {
                            error: "token proof without token auth".into(),
                        },
                        "protocol misuse",
                    );
                };
                let ProofSecret::Ecdh {
                    shared,
                    salt,
                    client_pub,
                    server_pub,
                } = secret
                else {
                    return Step::reject(
                        ControlMsg::AuthErr {
                            error: "token proof after SPAKE2 start".into(),
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
        ControlMsg::PairConfirm { .. } => "pair_confirm",
        ControlMsg::PairResult { .. } => "pair_result",
        ControlMsg::TokenProof { .. } => "token_proof",
        ControlMsg::AuthOk { .. } => "auth_ok",
        ControlMsg::AuthErr { .. } => "auth_err",
        _ => "post-auth message",
    }
}
