//! NDSP client SDK (Rust).
//!
//! Drives the full client side of the protocol over tokio-tungstenite:
//! Hello → (pairing with PIN | token reconnect) → encrypted session.
//! Pairing uses NDSP-PAKE v1 whenever the host advertises it (every host
//! since v0.3), falling back to the legacy PIN-HKDF handshake otherwise.
//! Used by the native desktop viewer and by the host's integration tests;
//! the web/Android/iOS clients implement the same flow in their own stacks.

use anyhow::{bail, Context};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use futures_util::{SinkExt, StreamExt};
use ndsp_protocol::{
    crypto::{self, HandshakeKeys},
    envelope::{Channel, Direction, Opener, Sealer},
    media::VideoFrame,
    messages::{AuthMethod, ClientInfo, Codec, ControlMsg, DisplayMode},
    pake::{PakeShare, PAKE_SUITE},
    PROTOCOL_VERSION, WS_PATH,
};
use tokio::net::TcpStream;
use tokio_tungstenite::{client_async, tungstenite::Message, WebSocketStream};

/// Object-safe stream bound so plain-TCP and TLS sockets share one type.
pub trait StreamIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin> StreamIo for T {}

type Ws = WebSocketStream<Box<dyn StreamIo>>;

/// Credential material persisted by a client after pairing.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub device_id: String,
    pub token: Vec<u8>,
    /// Host fingerprint seen at pairing; compare on reconnect.
    pub host_fingerprint: String,
}

pub enum Auth<'a> {
    /// First contact: pair with the PIN shown on the host (PAKE when the
    /// host advertises it — every current host — legacy HKDF otherwise).
    Pin(&'a str),
    /// Force the legacy PIN-HKDF pairing even against a PAKE-capable host
    /// (compat testing; mirrors what pre-PAKE viewers do).
    PinLegacy(&'a str),
    /// Returning device.
    Token(&'a Credentials),
}

/// How to reach the host.
#[derive(Debug, Clone, Default)]
pub enum Transport {
    /// Plain TCP WebSocket (`ws://`) — NDSP encrypts above the transport.
    #[default]
    Plain,
    /// TLS WebSocket (`wss://`) against a host running with `tls = true`,
    /// authenticated **only** by the pinned certificate fingerprint
    /// (lowercase hex SHA-256 of the DER cert, as printed by the host).
    TlsPinned { cert_sha256_hex: String },
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
    /// Set when this connection performed a fresh pairing.
    pub new_credentials: Option<Credentials>,
    pub server_fingerprint: String,
    /// True when pairing ran over the PAKE path (vs legacy PIN-HKDF).
    pub used_pake: bool,
}

/// Anything a session can yield to the app.
pub enum Incoming {
    Video(VideoFrame),
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
    connect_via(host, port, client, auth, codecs, Transport::Plain).await
}

pub async fn connect_via(
    host: &str,
    port: u16,
    client: ClientInfo,
    auth: Auth<'_>,
    codecs: Vec<Codec>,
    transport: Transport,
) -> anyhow::Result<Session> {
    let mut ws = open_websocket(host, port, &transport).await?;

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

    let (server_fingerprint, nonce, server_pake) = match recv_json(&mut ws).await? {
        ControlMsg::HelloAck {
            server,
            connection_nonce,
            pake,
            ..
        } => (
            server.fingerprint,
            B64.decode(connection_nonce).context("nonce b64")?,
            pake,
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

    let mut new_credentials = None;
    let mut used_pake = false;
    let session_key: [u8; 32];

    match &auth {
        Auth::Pin(pin) if server_pake.as_deref() == Some(PAKE_SUITE) => {
            // ---- PAKE pairing (NDSP-PAKE v1) --------------------------------
            used_pake = true;
            let share = PakeShare::generate(pin, &nonce, &client.device_id, &server_fingerprint)
                .map_err(|e| anyhow::anyhow!("PAKE share: {e}"))?;
            let client_share = share.public_bytes().to_vec();
            send_json(
                &mut ws,
                &ControlMsg::PakeStart {
                    client_pubkey: B64.encode(&client_share),
                },
            )
            .await?;
            let (server_share, salt) = match recv_json(&mut ws).await? {
                ControlMsg::PakeChallenge {
                    server_pubkey,
                    salt,
                } => (
                    B64.decode(server_pubkey).context("server share b64")?,
                    B64.decode(salt).context("salt b64")?,
                ),
                ControlMsg::AuthErr { error } => bail!("server rejected: {error}"),
                other => bail!("expected pake_challenge, got {other:?}"),
            };
            let secret = share
                .agree(&server_share, &nonce, &client_share, &server_share)
                .map_err(|e| anyhow::anyhow!("PAKE agree: {e}"))?;
            let pair_key = secret.pairing_key(&salt, &nonce);
            session_key = secret.session_key(&salt, &nonce);
            new_credentials = Some(
                confirm_pairing(&mut ws, pair_key, &nonce, &client, &server_fingerprint).await?,
            );
        }
        Auth::Pin(pin) | Auth::PinLegacy(pin) => {
            // ---- legacy PIN-HKDF pairing ------------------------------------
            let (shared, salt, _cp, _sp) = ephemeral_ecdh(&mut ws).await?;
            let pair_key = shared.pairing_key(&salt, pin, &nonce);
            session_key = shared.session_key(&salt, &nonce);
            new_credentials = Some(
                confirm_pairing(&mut ws, pair_key, &nonce, &client, &server_fingerprint).await?,
            );
        }
        Auth::Token(creds) => {
            // ---- token reconnect --------------------------------------------
            let (shared, salt, client_pub, server_pub) = ephemeral_ecdh(&mut ws).await?;
            session_key = shared.session_key(&salt, &nonce);
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

    let (codec, mode, input_allowed, clipboard_allowed) = match recv_json(&mut ws).await? {
        ControlMsg::AuthOk {
            codec,
            mode,
            input_allowed,
            clipboard_allowed,
        } => (codec, mode, input_allowed, clipboard_allowed),
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
        new_credentials,
        server_fingerprint,
        used_pake,
    })
}

/// The shared ephemeral-ECDH prelude of the legacy-pair and token paths.
/// Returns (shared secret, salt, client_pub, server_pub).
async fn ephemeral_ecdh(
    ws: &mut Ws,
) -> anyhow::Result<(crypto::SharedSecret, Vec<u8>, Vec<u8>, Vec<u8>)> {
    let keys = HandshakeKeys::generate();
    let client_pub = keys.public_bytes().to_vec();
    send_json(
        ws,
        &ControlMsg::PairStart {
            client_pubkey: B64.encode(&client_pub),
        },
    )
    .await?;
    let (server_pub, salt) = match recv_json(ws).await? {
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
    let shared = keys
        .agree(&server_pub)
        .map_err(|e| anyhow::anyhow!("ECDH: {e}"))?;
    Ok((shared, salt, client_pub, server_pub))
}

/// Prove PIN knowledge under `pair_key` and unseal the issued trust token.
async fn confirm_pairing(
    ws: &mut Ws,
    pair_key: [u8; 32],
    nonce: &[u8],
    client: &ClientInfo,
    server_fingerprint: &str,
) -> anyhow::Result<Credentials> {
    let mut confirm = crypto::CONFIRM_CONTEXT.to_vec();
    confirm.extend_from_slice(nonce);
    let sealed = crypto::seal(&pair_key, &confirm, b"");
    send_json(
        ws,
        &ControlMsg::PairConfirm {
            sealed: B64.encode(sealed),
        },
    )
    .await?;
    match recv_json(ws).await? {
        ControlMsg::PairResult {
            ok: true,
            sealed_token: Some(tok),
            ..
        } => {
            let sealed = B64.decode(tok).context("token b64")?;
            let token = crypto::open(&pair_key, &sealed, b"token")
                .map_err(|e| anyhow::anyhow!("token unseal: {e}"))?;
            Ok(Credentials {
                device_id: client.device_id.clone(),
                token,
                host_fingerprint: server_fingerprint.to_string(),
            })
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

/// TCP (+ optional pinned TLS) connect, then the WebSocket handshake.
async fn open_websocket(host: &str, port: u16, transport: &Transport) -> anyhow::Result<Ws> {
    let tcp = TcpStream::connect((host, port))
        .await
        .with_context(|| format!("connecting {host}:{port}"))?;
    // Input events must never sit in Nagle's buffer.
    let _ = tcp.set_nodelay(true);
    let (scheme, stream): (&str, Box<dyn StreamIo>) = match transport {
        Transport::Plain => ("ws", Box::new(tcp)),
        Transport::TlsPinned { cert_sha256_hex } => {
            let stream = tls_pinned::connect(tcp, host, cert_sha256_hex).await?;
            ("wss", Box::new(stream))
        }
    };
    let url = format!("{scheme}://{host}:{port}{WS_PATH}");
    let (ws, _) = client_async(&url, stream)
        .await
        .with_context(|| format!("websocket handshake {url}"))?;
    Ok(ws)
}

mod tls_pinned {
    //! Rustls client that authenticates the server **only** by certificate
    //! fingerprint — exactly what a self-signed per-install host cert needs
    //! (see host `tls = true` and `docs/SECURITY.md`).

    use anyhow::Context;
    use sha2::{Digest, Sha256};
    use std::sync::Arc;
    use tokio::net::TcpStream;
    use tokio_rustls::client::TlsStream;
    use tokio_rustls::rustls::client::danger::{
        HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
    };
    use tokio_rustls::rustls::crypto::{ring, verify_tls12_signature, verify_tls13_signature};
    use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use tokio_rustls::rustls::{ClientConfig, DigitallySignedStruct, Error, SignatureScheme};

    #[derive(Debug)]
    struct PinnedCertVerifier {
        pin: [u8; 32],
        provider: Arc<tokio_rustls::rustls::crypto::CryptoProvider>,
    }

    impl ServerCertVerifier for PinnedCertVerifier {
        fn verify_server_cert(
            &self,
            end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp_response: &[u8],
            _now: UnixTime,
        ) -> Result<ServerCertVerified, Error> {
            let got: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
            // Constant-time compare (paranoia; the pin is not secret).
            let mut diff = 0u8;
            for (a, b) in got.iter().zip(self.pin.iter()) {
                diff |= a ^ b;
            }
            if diff == 0 {
                Ok(ServerCertVerified::assertion())
            } else {
                Err(Error::General(format!(
                    "server certificate fingerprint mismatch (got sha256:{})",
                    hex::encode(got)
                )))
            }
        }

        fn verify_tls12_signature(
            &self,
            message: &[u8],
            cert: &CertificateDer<'_>,
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            verify_tls12_signature(
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
            dss: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, Error> {
            verify_tls13_signature(
                message,
                cert,
                dss,
                &self.provider.signature_verification_algorithms,
            )
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.provider
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    pub async fn connect(
        tcp: TcpStream,
        host: &str,
        cert_sha256_hex: &str,
    ) -> anyhow::Result<TlsStream<TcpStream>> {
        let pin_bytes = hex::decode(cert_sha256_hex.trim().trim_start_matches("sha256:"))
            .context("certificate pin must be hex")?;
        let pin: [u8; 32] = pin_bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("certificate pin must be 32 bytes of hex"))?;
        let provider = Arc::new(ring::default_provider());
        let config = ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .context("TLS versions")?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedCertVerifier { pin, provider }))
            .with_no_client_auth();
        let server_name =
            ServerName::try_from(host.to_string()).context("invalid TLS server name")?;
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        connector
            .connect(server_name, tcp)
            .await
            .context("TLS handshake (is the pinned fingerprint correct?)")
    }
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
                        Channel::Audio => continue, // reserved
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
