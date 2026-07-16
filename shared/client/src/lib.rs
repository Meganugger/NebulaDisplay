//! NDSP client SDK (Rust).
//!
//! Drives the full client side of the protocol over tokio-tungstenite:
//! Hello → (pairing with PIN | token reconnect) → encrypted session.
//! Used by the native desktop viewer and by the host's integration tests;
//! the web/Android/iOS clients implement the same flow in their own stacks.

use anyhow::{bail, Context};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use futures_util::{SinkExt, StreamExt};
use ndsp_protocol::{
    crypto::{self, HandshakeKeys},
    envelope::{Channel, Direction, Opener, Sealer},
    media::{AudioFrame, VideoFrame},
    messages::{AuthMethod, ClientInfo, Codec, ControlMsg, DisplayMode},
    pake::{Pake, PakeRole},
    PROTOCOL_VERSION, WS_PATH,
};
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Credential material persisted by a client after pairing.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub device_id: String,
    pub token: Vec<u8>,
    /// Host fingerprint seen at pairing; compare on reconnect.
    pub host_fingerprint: String,
}

pub enum Auth<'a> {
    /// First contact: pair with the PIN shown on the host via SPAKE2 (the
    /// transcript is not offline-grindable). Fails against pre-v0.5 hosts —
    /// use [`Auth::PinLegacy`] for those *explicitly* (automatic fallback
    /// would let an active MITM downgrade pairing to the grindable scheme).
    Pin(&'a str),
    /// First contact against a pre-v0.5 host: legacy PIN-bound-HKDF pairing.
    PinLegacy(&'a str),
    /// Returning device.
    Token(&'a Credentials),
}

/// An authenticated, encrypted session.
pub struct Session {
    ws: Ws,
    sealer: Sealer,
    opener: Opener,
    pub codec: Codec,
    pub mode: DisplayMode,
    pub input_allowed: bool,
    pub clipboard_allowed: bool,
    /// Host can stream audio (still requires an explicit `SetAudio` opt-in).
    pub audio_available: bool,
    /// Set when this connection performed a fresh pairing.
    pub new_credentials: Option<Credentials>,
    pub server_fingerprint: String,
}

/// Anything a session can yield to the app.
pub enum Incoming {
    Video(VideoFrame),
    Audio(AudioFrame),
    Control(ControlMsg),
    Closed,
}

pub async fn connect(
    host: &str,
    port: u16,
    client: ClientInfo,
    auth: Auth<'_>,
    codecs: Vec<Codec>,
) -> anyhow::Result<Session> {
    let url = format!("ws://{host}:{port}{WS_PATH}");
    let (mut ws, _) = connect_async(&url)
        .await
        .with_context(|| format!("connecting {url}"))?;

    let auth_method = match &auth {
        Auth::Pin(_) | Auth::PinLegacy(_) => AuthMethod::Pair,
        Auth::Token(c) => AuthMethod::Token {
            device_id: c.device_id.clone(),
        },
    };
    send_json(
        &mut ws,
        &ControlMsg::Hello {
            protocol: PROTOCOL_VERSION,
            client: client.clone(),
            auth: auth_method,
            codecs,
        },
    )
    .await?;

    let (server_fingerprint, nonce) = match recv_json(&mut ws).await? {
        ControlMsg::HelloAck {
            server,
            connection_nonce,
            ..
        } => (
            server.fingerprint,
            B64.decode(connection_nonce).context("nonce b64")?,
        ),
        other => bail!("expected hello_ack, got {other:?}"),
    };

    if let Auth::Token(c) = &auth {
        if c.host_fingerprint != server_fingerprint {
            bail!(
                "host fingerprint changed ({} != {}); refusing to send token proof — re-pair explicitly",
                &server_fingerprint[..16.min(server_fingerprint.len())],
                &c.host_fingerprint[..16.min(c.host_fingerprint.len())]
            );
        }
    }

    // Ephemeral ECDH (both auth paths).
    let keys = HandshakeKeys::generate();
    let client_pub = keys.public_bytes().to_vec();
    send_json(
        &mut ws,
        &ControlMsg::PairStart {
            client_pubkey: B64.encode(&client_pub),
            pake: matches!(auth, Auth::Pin(_)),
        },
    )
    .await?;
    let (server_pub, salt, server_pake_share) = match recv_json(&mut ws).await? {
        ControlMsg::PairChallenge {
            server_pubkey,
            salt,
            pake_share,
        } => (
            B64.decode(server_pubkey).context("server pub b64")?,
            B64.decode(salt).context("salt b64")?,
            pake_share
                .map(|s| B64.decode(s).context("pake share b64"))
                .transpose()?,
        ),
        ControlMsg::AuthErr { error } => bail!("server rejected: {error}"),
        other => bail!("expected pair_challenge, got {other:?}"),
    };
    let shared = keys
        .agree(&server_pub)
        .map_err(|e| anyhow::anyhow!("ECDH: {e}"))?;
    let session_key = shared.session_key(&salt, &nonce);

    let mut new_credentials = None;
    match auth {
        Auth::Pin(pin) | Auth::PinLegacy(pin) => {
            // Derive the pairing key: SPAKE2 when negotiated, legacy PIN-HKDF
            // only when the caller explicitly opted into it.
            let (pair_key, client_pake_share) = match (&auth, server_pake_share) {
                (Auth::Pin(_), Some(server_share)) => {
                    let pake = Pake::new(PakeRole::Client, pin, &salt, &nonce);
                    let my_share = pake.share().to_vec();
                    let key = pake
                        .finish(&server_share, &client_pub, &server_pub)
                        .map_err(|e| anyhow::anyhow!("PAKE: {e}"))?;
                    (key, Some(B64.encode(my_share)))
                }
                (Auth::Pin(_), None) => bail!(
                    "host did not offer PAKE pairing (pre-v0.5?); refusing the \
                     downgradable legacy path — use PinLegacy explicitly if you \
                     trust this network"
                ),
                _ => (shared.pairing_key(&salt, pin, &nonce), None),
            };
            let mut confirm = crypto::CONFIRM_CONTEXT.to_vec();
            confirm.extend_from_slice(&nonce);
            let sealed = crypto::seal(&pair_key, &confirm, b"");
            send_json(
                &mut ws,
                &ControlMsg::PairConfirm {
                    sealed: B64.encode(sealed),
                    pake_share: client_pake_share,
                },
            )
            .await?;
            match recv_json(&mut ws).await? {
                ControlMsg::PairResult {
                    ok: true,
                    sealed_token: Some(tok),
                    ..
                } => {
                    let sealed = B64.decode(tok).context("token b64")?;
                    let token = crypto::open(&pair_key, &sealed, b"token")
                        .map_err(|e| anyhow::anyhow!("token unseal: {e}"))?;
                    new_credentials = Some(Credentials {
                        device_id: client.device_id.clone(),
                        token,
                        host_fingerprint: server_fingerprint.clone(),
                    });
                }
                ControlMsg::PairResult {
                    ok: false, error, ..
                } => {
                    bail!(
                        "pairing failed: {}",
                        error.unwrap_or_else(|| "unknown".into())
                    )
                }
                other => bail!("expected pair_result, got {other:?}"),
            }
        }
        Auth::Token(creds) => {
            let transcript = crypto::reauth_transcript(&nonce, &client_pub, &server_pub);
            let proof = crypto::token_proof(&creds.token, &transcript);
            send_json(
                &mut ws,
                &ControlMsg::TokenProof {
                    proof: B64.encode(proof),
                },
            )
            .await?;
        }
    }

    let (codec, mode, input_allowed, clipboard_allowed, audio_available) =
        match recv_json(&mut ws).await? {
            ControlMsg::AuthOk {
                codec,
                mode,
                input_allowed,
                clipboard_allowed,
                audio_available,
            } => (
                codec,
                mode,
                input_allowed,
                clipboard_allowed,
                audio_available,
            ),
            ControlMsg::AuthErr { error } => bail!("auth rejected: {error}"),
            other => bail!("expected auth_ok, got {other:?}"),
        };

    Ok(Session {
        ws,
        sealer: Sealer::new(&session_key, Direction::ClientToServer),
        opener: Opener::new(&session_key, Direction::ServerToClient),
        codec,
        mode,
        input_allowed,
        clipboard_allowed,
        audio_available,
        new_credentials,
        server_fingerprint,
    })
}

impl Session {
    pub async fn send(&mut self, msg: &ControlMsg) -> anyhow::Result<()> {
        let json = msg.to_json()?;
        let env = self.sealer.seal(Channel::Control, json.as_bytes());
        self.ws.send(Message::Binary(env.into())).await?;
        Ok(())
    }

    /// Receive the next decrypted item (video frame or control message).
    pub async fn recv(&mut self) -> anyhow::Result<Incoming> {
        loop {
            let Some(msg) = self.ws.next().await else {
                return Ok(Incoming::Closed);
            };
            match msg? {
                Message::Binary(data) => {
                    let (chan, pt) = self
                        .opener
                        .open(&data)
                        .map_err(|e| anyhow::anyhow!("envelope: {e}"))?;
                    match chan {
                        Channel::Video => {
                            return Ok(Incoming::Video(
                                VideoFrame::decode(&pt)
                                    .map_err(|e| anyhow::anyhow!("frame: {e}"))?,
                            ))
                        }
                        Channel::Control => {
                            let s = std::str::from_utf8(&pt).context("control utf8")?;
                            return Ok(Incoming::Control(ControlMsg::from_json(s)?));
                        }
                        Channel::Audio => {
                            return Ok(Incoming::Audio(
                                AudioFrame::decode(&pt)
                                    .map_err(|e| anyhow::anyhow!("audio frame: {e}"))?,
                            ))
                        }
                    }
                }
                Message::Close(_) => return Ok(Incoming::Closed),
                _ => continue,
            }
        }
    }

    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }
}

pub fn default_client_info(name: &str, platform: &str) -> ClientInfo {
    ClientInfo {
        device_id: uuid::Uuid::new_v4().to_string(),
        name: name.to_string(),
        platform: platform.to_string(),
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        features: Vec::new(),
    }
}

async fn send_json(ws: &mut Ws, msg: &ControlMsg) -> anyhow::Result<()> {
    ws.send(Message::Text(msg.to_json()?.into())).await?;
    Ok(())
}

async fn recv_json(ws: &mut Ws) -> anyhow::Result<ControlMsg> {
    loop {
        let Some(msg) = ws.next().await else {
            bail!("connection closed during handshake")
        };
        match msg? {
            Message::Text(t) => return Ok(ControlMsg::from_json(&t)?),
            Message::Close(frame) => bail!("server closed connection during handshake: {frame:?}"),
            Message::Ping(_) | Message::Pong(_) => continue,
            other => bail!("unexpected message during handshake: {other:?}"),
        }
    }
}
