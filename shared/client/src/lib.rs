//! NDSP client SDK (Rust).
//!
//! Drives the full client side of the protocol — Hello → (pairing with PIN
//! | token reconnect) → encrypted session — over either transport:
//!
//! * **WebSocket** ([`connect`], tokio-tungstenite): works everywhere.
//! * **QUIC** ([`connect_quic`], quinn): same handshake and envelopes on a
//!   bidirectional control stream; video arrives on per-frame
//!   unidirectional streams (no head-of-line blocking across frames on
//!   lossy links — late frames are dropped as stale by the envelope
//!   counters), audio on one ordered unidirectional stream. The QUIC TLS
//!   certificate is *not* verified: exactly like `ws://`, all security
//!   comes from the NDSP layer (see the host's `docs/SECURITY.md`).
//!
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
    spake2::{mac_equal, Spake2Client},
    PROTOCOL_VERSION, WS_PATH,
};
use tokio::net::TcpStream;
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// NDSP-over-QUIC ALPN (must match the host).
pub const QUIC_ALPN: &[u8] = b"ndsp/1";
/// QUIC control-stream frame types.
const FRAME_TEXT: u8 = 0;
const FRAME_ENVELOPE: u8 = 1;
/// Caps for inbound QUIC payloads.
const MAX_CONTROL_FRAME: usize = 4 * 1024 * 1024;
const MAX_MEDIA_FRAME: usize = 32 * 1024 * 1024;

/// Credential material persisted by a client after pairing.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub device_id: String,
    pub token: Vec<u8>,
    /// Host fingerprint seen at pairing; compare on reconnect.
    pub host_fingerprint: String,
}

pub enum Auth<'a> {
    /// First contact: pair with the PIN shown on the host (SPAKE2 — the
    /// recorded transcript cannot be ground offline against the PIN).
    Pin(&'a str),
    /// First contact using the legacy PIN-bound-HKDF scheme (what the
    /// current mobile apps speak; hosts may disable it).
    PinLegacy(&'a str),
    /// Returning device.
    Token(&'a Credentials),
}

/// An authenticated, encrypted session.
pub struct Session {
    transport: ClientTransport,
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
    let url = format!("ws://{host}:{port}{WS_PATH}");
    let (ws, _) = connect_async(&url)
        .await
        .with_context(|| format!("connecting {url}"))?;
    handshake(Wire::Ws(ws), client, auth, codecs).await
}

/// Connect over QUIC (UDP, same port number). Same handshake, same
/// [`Session`] API; see the module docs for the transport mapping.
pub async fn connect_quic(
    host: &str,
    port: u16,
    client: ClientInfo,
    auth: Auth<'_>,
    codecs: Vec<Codec>,
) -> anyhow::Result<Session> {
    let addr = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("resolving {host}:{port}"))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("{host}: no address"))?;
    let bind: std::net::SocketAddr = if addr.is_ipv6() {
        "[::]:0".parse().unwrap()
    } else {
        "0.0.0.0:0".parse().unwrap()
    };
    let mut endpoint = quinn::Endpoint::client(bind)?;
    endpoint.set_default_client_config(quic_client_config()?);
    let conn = endpoint
        .connect(addr, host)
        .with_context(|| format!("quic connect {addr}"))?
        .await
        .with_context(|| format!("quic handshake {addr}"))?;
    // The control stream: opened by us, first frame is the Hello.
    let (send, recv) = conn.open_bi().await.context("opening control stream")?;
    handshake(
        Wire::Quic {
            _endpoint: endpoint,
            conn,
            send,
            recv,
        },
        client,
        auth,
        codecs,
    )
    .await
}

async fn handshake(
    mut ws: Wire,
    client: ClientInfo,
    auth: Auth<'_>,
    codecs: Vec<Codec>,
) -> anyhow::Result<Session> {
    let auth_method = match &auth {
        Auth::Pin(_) => AuthMethod::PairSpake2,
        Auth::PinLegacy(_) => AuthMethod::Pair,
        Auth::Token(c) => AuthMethod::Token {
            device_id: c.device_id.clone(),
        },
    };
    ws.send_json(&ControlMsg::Hello {
        protocol: PROTOCOL_VERSION,
        client: client.clone(),
        auth: auth_method,
        codecs,
    })
    .await?;

    let (server_fingerprint, nonce) = match ws.recv_json().await? {
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

    let mut new_credentials = None;
    let session_key: [u8; 32];

    if let Auth::Pin(pin) = &auth {
        // ---- SPAKE2 pairing (no separate ECDH needed: the PAKE itself
        // yields a fresh session key per connection) -----------------------
        let pake = Spake2Client::start(pin, &nonce);
        ws.send_json(&ControlMsg::Spake2Start {
            share: B64.encode(pake.share()),
        })
        .await?;
        let server_share = match ws.recv_json().await? {
            ControlMsg::Spake2Challenge { share } => {
                B64.decode(share).context("server share b64")?
            }
            ControlMsg::AuthErr { error } => bail!("server rejected: {error}"),
            other => bail!("expected spake2_challenge, got {other:?}"),
        };
        let keys = pake
            .finish(&server_share)
            .map_err(|e| anyhow::anyhow!("SPAKE2: {e}"))?;
        ws.send_json(&ControlMsg::Spake2Confirm {
            mac: B64.encode(keys.confirm_client),
        })
        .await?;
        match ws.recv_json().await? {
            ControlMsg::Spake2Result {
                ok: true,
                mac: Some(mac),
                sealed_token: Some(tok),
                ..
            } => {
                // Mutual authentication: the server must prove it knew the
                // PIN too before we accept anything from it.
                let mac = B64.decode(mac).context("server mac b64")?;
                if !mac_equal(&mac, &keys.confirm_server) {
                    bail!("server failed SPAKE2 confirmation — possible MITM; aborting");
                }
                let sealed = B64.decode(tok).context("token b64")?;
                let token = crypto::open(&keys.token_key, &sealed, b"token")
                    .map_err(|e| anyhow::anyhow!("token unseal: {e}"))?;
                new_credentials = Some(Credentials {
                    device_id: client.device_id.clone(),
                    token,
                    host_fingerprint: server_fingerprint.clone(),
                });
            }
            ControlMsg::Spake2Result {
                ok: false, error, ..
            } => bail!(
                "pairing failed: {}",
                error.unwrap_or_else(|| "unknown".into())
            ),
            other => bail!("expected spake2_result, got {other:?}"),
        }
        session_key = keys.session_key;
    } else {
        // ---- Ephemeral ECDH (legacy pairing + token reconnect) ------------
        let keys = HandshakeKeys::generate();
        let client_pub = keys.public_bytes().to_vec();
        ws.send_json(&ControlMsg::PairStart {
            client_pubkey: B64.encode(&client_pub),
        })
        .await?;
        let (server_pub, salt) = match ws.recv_json().await? {
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
        session_key = shared.session_key(&salt, &nonce);

        match auth {
            Auth::PinLegacy(pin) => {
                let pair_key = shared.pairing_key(&salt, pin, &nonce);
                let mut confirm = crypto::CONFIRM_CONTEXT.to_vec();
                confirm.extend_from_slice(&nonce);
                let sealed = crypto::seal(&pair_key, &confirm, b"");
                ws.send_json(&ControlMsg::PairConfirm {
                    sealed: B64.encode(sealed),
                })
                .await?;
                match ws.recv_json().await? {
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
                ws.send_json(&ControlMsg::TokenProof {
                    proof: B64.encode(proof),
                })
                .await?;
            }
            Auth::Pin(_) => unreachable!("handled above"),
        }
    }

    let (codec, mode, input_allowed) = match ws.recv_json().await? {
        ControlMsg::AuthOk {
            codec,
            mode,
            input_allowed,
        } => (codec, mode, input_allowed),
        ControlMsg::AuthErr { error } => bail!("auth rejected: {error}"),
        other => bail!("expected auth_ok, got {other:?}"),
    };

    Ok(Session {
        transport: ws.into_transport(),
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
        match &mut self.transport {
            ClientTransport::Ws(ws) => ws.send(Message::Binary(env.into())).await?,
            ClientTransport::Quic { control, .. } => {
                write_quic_frame(control, FRAME_ENVELOPE, &env).await?
            }
        }
        Ok(())
    }

    /// Receive the next decrypted item (video frame or control message).
    pub async fn recv(&mut self) -> anyhow::Result<Incoming> {
        loop {
            let data: Vec<u8> = match &mut self.transport {
                ClientTransport::Ws(ws) => {
                    let Some(msg) = ws.next().await else {
                        return Ok(Incoming::Closed);
                    };
                    match msg? {
                        Message::Binary(data) => data.to_vec(),
                        Message::Close(_) => return Ok(Incoming::Closed),
                        _ => continue,
                    }
                }
                ClientTransport::Quic { inbound, .. } => {
                    let Some(data) = inbound.recv().await else {
                        return Ok(Incoming::Closed);
                    };
                    data
                }
            };
            let opened = self.opener.open(&data);
            // QUIC delivers per-frame video streams in completion order; a
            // frame overtaken by a newer one trips the monotonic-counter
            // check. That *is* the latest-only contract — drop it silently.
            if matches!(self.transport, ClientTransport::Quic { .. })
                && matches!(opened, Err(ndsp_protocol::ProtocolError::Replay))
                && data.first() == Some(&(Channel::Video as u8))
            {
                continue;
            }
            let (chan, pt) = opened.map_err(|e| anyhow::anyhow!("envelope: {e}"))?;
            match chan {
                Channel::Video => {
                    return Ok(Incoming::Video(
                        VideoFrame::decode(&pt).map_err(|e| anyhow::anyhow!("frame: {e}"))?,
                    ))
                }
                Channel::Control => {
                    let s = std::str::from_utf8(&pt).context("control utf8")?;
                    return Ok(Incoming::Control(ControlMsg::from_json(s)?));
                }
                Channel::Audio => {
                    return Ok(Incoming::Audio(
                        AudioFrame::decode(&pt).map_err(|e| anyhow::anyhow!("audio frame: {e}"))?,
                    ))
                }
            }
        }
    }

    pub async fn close(mut self) {
        match &mut self.transport {
            ClientTransport::Ws(ws) => {
                let _ = ws.close(None).await;
            }
            ClientTransport::Quic { conn, .. } => conn.close(0u32.into(), b"bye"),
        }
    }
}

// --- transport plumbing -------------------------------------------------------

/// Handshake-phase wire (plaintext JSON messages).
enum Wire {
    Ws(Ws),
    Quic {
        /// Kept alive for the life of the connection.
        _endpoint: quinn::Endpoint,
        conn: quinn::Connection,
        send: quinn::SendStream,
        recv: quinn::RecvStream,
    },
}

impl Wire {
    async fn send_json(&mut self, msg: &ControlMsg) -> anyhow::Result<()> {
        let json = msg.to_json()?;
        match self {
            Wire::Ws(ws) => ws.send(Message::Text(json.into())).await?,
            Wire::Quic { send, .. } => write_quic_frame(send, FRAME_TEXT, json.as_bytes()).await?,
        }
        Ok(())
    }

    async fn recv_json(&mut self) -> anyhow::Result<ControlMsg> {
        match self {
            Wire::Ws(ws) => loop {
                let Some(msg) = ws.next().await else {
                    bail!("connection closed during handshake")
                };
                match msg? {
                    Message::Text(t) => return Ok(ControlMsg::from_json(&t)?),
                    Message::Close(frame) => {
                        bail!("server closed connection during handshake: {frame:?}")
                    }
                    Message::Ping(_) | Message::Pong(_) => continue,
                    other => bail!("unexpected message during handshake: {other:?}"),
                }
            },
            Wire::Quic { recv, .. } => match read_quic_frame(recv, MAX_CONTROL_FRAME).await? {
                Some((FRAME_TEXT, payload)) => {
                    Ok(ControlMsg::from_json(std::str::from_utf8(&payload)?)?)
                }
                Some((t, _)) => bail!("unexpected frame type {t} during handshake"),
                None => bail!("server closed connection during handshake"),
            },
        }
    }

    /// Post-auth: become the session transport (QUIC spawns its demux).
    fn into_transport(self) -> ClientTransport {
        match self {
            Wire::Ws(ws) => ClientTransport::Ws(ws),
            Wire::Quic {
                _endpoint,
                conn,
                send,
                recv,
            } => {
                let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
                tokio::spawn(demux(conn.clone(), recv, tx));
                ClientTransport::Quic {
                    _endpoint,
                    conn,
                    control: send,
                    inbound: rx,
                }
            }
        }
    }
}

/// Post-auth session transport.
enum ClientTransport {
    Ws(Ws),
    Quic {
        _endpoint: quinn::Endpoint,
        conn: quinn::Connection,
        control: quinn::SendStream,
        /// Envelopes from the control stream + all unidirectional streams,
        /// merged by the demux task. Channel closed = connection gone.
        inbound: tokio::sync::mpsc::Receiver<Vec<u8>>,
    },
}

/// Merge control-stream envelopes and server-opened unidirectional streams
/// (per-frame video, ordered audio) into one inbound queue.
async fn demux(
    conn: quinn::Connection,
    mut control: quinn::RecvStream,
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
) {
    loop {
        tokio::select! {
            frame = read_quic_frame(&mut control, MAX_CONTROL_FRAME) => {
                match frame {
                    Ok(Some((FRAME_ENVELOPE, env))) => {
                        if tx.send(env).await.is_err() { return; }
                    }
                    _ => return, // FIN, violation or connection loss
                }
            }
            uni = conn.accept_uni() => {
                let Ok(stream) = uni else { return };
                tokio::spawn(read_uni(stream, tx.clone()));
            }
        }
    }
}

/// Drain one server-opened unidirectional stream ('V' = single video frame,
/// 'A' = ordered audio frames until FIN).
async fn read_uni(mut stream: quinn::RecvStream, tx: tokio::sync::mpsc::Sender<Vec<u8>>) {
    let mut tag = [0u8; 1];
    if stream.read_exact(&mut tag).await.is_err() {
        return;
    }
    match tag[0] {
        b'V' => {
            if let Ok(Some((_, env))) = read_quic_frame_untyped(&mut stream, MAX_MEDIA_FRAME).await
            {
                let _ = tx.send(env).await;
            }
        }
        b'A' => {
            while let Ok(Some((_, env))) =
                read_quic_frame_untyped(&mut stream, MAX_MEDIA_FRAME).await
            {
                if tx.send(env).await.is_err() {
                    return;
                }
            }
        }
        other => {
            tracing_lite_warn(&format!("unknown uni-stream tag {other}"));
        }
    }
}

/// The SDK avoids a hard tracing dependency; warnings are rare.
fn tracing_lite_warn(msg: &str) {
    eprintln!("ndsp-client: {msg}");
}

/// `[type u8][len u32 BE][payload]` framing on QUIC streams.
async fn write_quic_frame(
    stream: &mut quinn::SendStream,
    frame_type: u8,
    payload: &[u8],
) -> anyhow::Result<()> {
    let mut head = [0u8; 5];
    head[0] = frame_type;
    head[1..5].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    stream.write_all(&head).await?;
    stream.write_all(payload).await?;
    Ok(())
}

async fn read_quic_frame(
    stream: &mut quinn::RecvStream,
    max_len: usize,
) -> anyhow::Result<Option<(u8, Vec<u8>)>> {
    let mut head = [0u8; 5];
    match stream.read_exact(&mut head).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(head[1..5].try_into().unwrap()) as usize;
    anyhow::ensure!(len <= max_len, "frame length {len} exceeds cap");
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    Ok(Some((head[0], payload)))
}

/// Media streams carry `[len u32 BE][envelope]` (no type byte after the tag).
async fn read_quic_frame_untyped(
    stream: &mut quinn::RecvStream,
    max_len: usize,
) -> anyhow::Result<Option<((), Vec<u8>)>> {
    let mut head = [0u8; 4];
    match stream.read_exact(&mut head).await {
        Ok(()) => {}
        Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(head) as usize;
    anyhow::ensure!(len <= max_len, "frame length {len} exceeds cap");
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    Ok(Some(((), payload)))
}

/// QUIC client config: TLS 1.3 with certificate verification disabled — the
/// NDSP layer authenticates the host (fingerprint + SPAKE2/token binding),
/// identical to the ws:// trust model. See the host's docs/SECURITY.md.
fn quic_client_config() -> anyhow::Result<quinn::ClientConfig> {
    let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
    let mut crypto = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(SkipServerVerification(provider)))
        .with_no_client_auth();
    crypto.alpn_protocols = vec![QUIC_ALPN.to_vec()];
    let mut config = quinn::ClientConfig::new(std::sync::Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?,
    ));
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(std::time::Duration::from_secs(30).try_into()?));
    transport.keep_alive_interval(Some(std::time::Duration::from_secs(5)));
    config.transport_config(std::sync::Arc::new(transport));
    Ok(config)
}

#[derive(Debug)]
struct SkipServerVerification(std::sync::Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
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
