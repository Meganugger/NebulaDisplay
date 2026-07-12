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
    pake::Spake2Client,
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

#[derive(Clone, Copy)]
pub enum Auth<'a> {
    /// First contact: pair with the PIN shown on the host. Uses SPAKE2
    /// (PAKE — no offline PIN grinding from transcripts) and transparently
    /// falls back to the legacy method against pre-SPAKE2 hosts.
    Pin(&'a str),
    /// First contact, forcing the legacy PIN-bound-HKDF pairing (testing /
    /// very old hosts).
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
    connect_with_tls(host, port, client, auth, codecs, None).await
}

/// Like [`connect`], but over TLS (`wss://`) when `tls` is set. The host's
/// self-signed certificate is accepted **only** if its SHA-256 matches the
/// pinned fingerprint the host printed at startup — a wrong or substituted
/// certificate aborts before any NDSP message is sent.
pub async fn connect_with_tls(
    host: &str,
    port: u16,
    client: ClientInfo,
    auth: Auth<'_>,
    codecs: Vec<Codec>,
    tls: Option<&TlsPin>,
) -> anyhow::Result<Session> {
    if let Auth::Pin(pin) = auth {
        // Prefer SPAKE2. A pre-SPAKE2 host cannot parse the auth method and
        // drops the socket during the handshake — retry once with legacy.
        return match connect_once(
            host,
            port,
            client.clone(),
            Auth::Pin(pin),
            codecs.clone(),
            tls,
        )
        .await
        {
            Err(e) if handshake_dropped(&e) => {
                tracing::warn!(
                    "host closed during SPAKE2 pairing (pre-PAKE host?); retrying legacy pairing"
                );
                connect_once(host, port, client, Auth::PinLegacy(pin), codecs, tls).await
            }
            r => r,
        };
    }
    connect_once(host, port, client, auth, codecs, tls).await
}

/// Certificate pin for [`connect_with_tls`]: the SHA-256 (hex) of the host's
/// HTTPS certificate, as printed in the host banner / shown in the panel.
pub struct TlsPin {
    pub cert_sha256_hex: String,
}

#[cfg(feature = "tls")]
mod pinned_tls {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use sha2::{Digest, Sha256};

    /// Accepts exactly one certificate: the one whose SHA-256 was pinned.
    /// Chain, expiry, and hostname are irrelevant — the pin *is* the trust
    /// statement (same model as SSH known_hosts).
    #[derive(Debug)]
    pub(super) struct PinVerifier {
        pub expected_sha256: [u8; 32],
        pub provider: std::sync::Arc<rustls::crypto::CryptoProvider>,
    }

    impl ServerCertVerifier for PinVerifier {
        fn verify_server_cert(
            &self,
            end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            let got: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
            if got == self.expected_sha256 {
                Ok(ServerCertVerified::assertion())
            } else {
                Err(rustls::Error::General(format!(
                    "certificate fingerprint mismatch (got {}, pinned {})",
                    hex::encode(got),
                    hex::encode(self.expected_sha256)
                )))
            }
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn verify_tls13_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &rustls::DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            self.provider
                .signature_verification_algorithms
                .supported_schemes()
        }
    }
}

#[cfg(feature = "tls")]
fn pinned_tls_config(pin: &TlsPin) -> anyhow::Result<rustls::ClientConfig> {
    let expected: Vec<u8> = hex::decode(pin.cert_sha256_hex.trim())
        .context("TLS pin must be the hex SHA-256 of the host certificate")?;
    let expected_sha256: [u8; 32] = expected
        .try_into()
        .map_err(|_| anyhow::anyhow!("TLS pin must be 32 bytes (64 hex chars)"))?;
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()?
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(pinned_tls::PinVerifier {
            expected_sha256,
            provider,
        }))
        .with_no_client_auth();
    Ok(config)
}

/// True when the failure looks like "the server dropped the socket during
/// the plaintext handshake" (what an older host does on an unknown auth
/// method) rather than an explicit rejection like a wrong PIN.
fn handshake_dropped(e: &anyhow::Error) -> bool {
    let msg = format!("{e:#}").to_lowercase();
    msg.contains("closed") && msg.contains("handshake")
}

async fn connect_once(
    host: &str,
    port: u16,
    client: ClientInfo,
    auth: Auth<'_>,
    codecs: Vec<Codec>,
    tls: Option<&TlsPin>,
) -> anyhow::Result<Session> {
    let scheme = if tls.is_some() { "wss" } else { "ws" };
    let url = format!("{scheme}://{host}:{port}{WS_PATH}");
    let (mut ws, _) = match tls {
        #[cfg(feature = "tls")]
        Some(pin) => {
            let config = pinned_tls_config(pin)?;
            tokio_tungstenite::connect_async_tls_with_config(
                &url,
                None,
                false,
                Some(tokio_tungstenite::Connector::Rustls(std::sync::Arc::new(
                    config,
                ))),
            )
            .await
            .with_context(|| format!("connecting {url} (TLS, pinned cert)"))?
        }
        #[cfg(not(feature = "tls"))]
        Some(_) => anyhow::bail!("this build lacks the `tls` feature (pinned TLS unavailable)"),
        None => connect_async(&url)
            .await
            .with_context(|| format!("connecting {url}"))?,
    };

    let auth_method = match &auth {
        Auth::Pin(_) => AuthMethod::PairSpake2,
        Auth::PinLegacy(_) => AuthMethod::Pair,
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

    // First flight: SPAKE2 element for PAKE pairing, ephemeral ECDH public
    // key for the legacy/token paths (same message either way).
    enum FirstFlight<'p> {
        Spake2(Spake2Client),
        Ecdh(HandshakeKeys, Auth<'p>),
    }
    let flight = match auth {
        Auth::Pin(pin) => FirstFlight::Spake2(Spake2Client::start(pin, &nonce)),
        other => FirstFlight::Ecdh(HandshakeKeys::generate(), other),
    };
    let client_pub = match &flight {
        FirstFlight::Spake2(c) => c.public_bytes().to_vec(),
        FirstFlight::Ecdh(k, _) => k.public_bytes().to_vec(),
    };
    send_json(
        &mut ws,
        &ControlMsg::PairStart {
            client_pubkey: B64.encode(&client_pub),
        },
    )
    .await?;
    let (server_pub, salt) = match recv_json(&mut ws).await? {
        ControlMsg::PairChallenge {
            server_pubkey,
            salt,
        } => (
            B64.decode(server_pubkey).context("server pub b64")?,
            B64.decode(salt).context("salt b64")?,
        ),
        ControlMsg::AuthErr { error } => bail!("server rejected: {error}"),
        other => bail!("expected pair_challenge, got {other:?}"),
    };

    // Second flight: prove the PIN (pairing) or the trust token (reconnect),
    // and settle on the session key.
    let mut new_credentials = None;
    let session_key: [u8; 32];
    let pair_key: Option<[u8; 32]> = match flight {
        FirstFlight::Spake2(c) => {
            let keys = c
                .finish(&server_pub, &nonce, &salt)
                .map_err(|e| anyhow::anyhow!("SPAKE2: {e}"))?;
            session_key = keys.session_key;
            Some(keys.pairing_key)
        }
        FirstFlight::Ecdh(keys, auth) => {
            let shared = keys
                .agree(&server_pub)
                .map_err(|e| anyhow::anyhow!("ECDH: {e}"))?;
            session_key = shared.session_key(&salt, &nonce);
            match auth {
                Auth::PinLegacy(pin) => Some(shared.pairing_key(&salt, pin, &nonce)),
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
                    None
                }
                Auth::Pin(_) => unreachable!("Pin always takes the SPAKE2 flight"),
            }
        }
    };

    if let Some(pair_key) = pair_key {
        let mut confirm = crypto::CONFIRM_CONTEXT.to_vec();
        confirm.extend_from_slice(&nonce);
        let sealed = crypto::seal(&pair_key, &confirm, b"");
        send_json(
            &mut ws,
            &ControlMsg::PairConfirm {
                sealed: B64.encode(sealed),
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

    let (codec, mode, input_allowed) = match recv_json(&mut ws).await? {
        ControlMsg::AuthOk {
            codec,
            mode,
            input_allowed,
        } => (codec, mode, input_allowed),
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
