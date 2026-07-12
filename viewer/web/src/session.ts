// NDSP session: connect → (pair | token reconnect) → encrypted session.

import { caps, probeH264Decode, probeHevcDecode } from "./caps";
import {
  b64decode,
  b64encode,
  CONFIRM_CONTEXT,
  agree,
  clearCredentials,
  deviceId,
  generateHandshakeKeys,
  importAesKey,
  loadCredentials,
  open,
  pairingKeyPake,
  saveCredentials,
  seal,
  sessionKeyBytes,
  sessionKeyBytesPake,
  tokenProof,
} from "./crypto";
import { pakeStart } from "./pake";
import {
  AudioFrameMsg,
  CHANNEL_AUDIO,
  CHANNEL_CONTROL,
  CHANNEL_FILE,
  CHANNEL_VIDEO,
  ControlMsg,
  DIR_CLIENT_TO_SERVER,
  DIR_SERVER_TO_CLIENT,
  DisplayMode,
  encodeFileChunkHeader,
  Opener,
  parseAudioFrame,
  parseVideoFrame,
  PROTOCOL_VERSION,
  Sealer,
  td,
  te,
  VideoFrame,
  WS_PATH,
} from "./protocol";

export interface SessionEvents {
  onVideo(frame: VideoFrame): void;
  onAudio(frame: AudioFrameMsg): void;
  onControl(msg: ControlMsg): void;
  onClose(reason: string): void;
}

export interface SessionInfo {
  codec: string;
  mode: DisplayMode;
  inputAllowed: boolean;
  clipboardAllowed: boolean;
  audioAvailable: boolean;
  serverName: string;
  fingerprint: string;
  newlyPaired: boolean;
}

export class Session {
  private constructor(
    private ws: WebSocket,
    private sealer: Sealer,
    public readonly info: SessionInfo,
  ) {}

  /**
   * Connect and authenticate. Tries stored credentials first; falls back to
   * PIN pairing when `pin` is provided.
   */
  static async connect(
    host: string,
    pin: string | null,
    clientName: string,
    events: SessionEvents,
  ): Promise<Session> {
    const stored = loadCredentials(host);
    if (!stored && !pin) throw new Error("PIN required for first-time pairing");

    // Match the page's security level: an https page may not open ws://.
    const scheme =
      typeof location !== "undefined" && location.protocol === "https:" ? "wss" : "ws";
    const url = `${scheme}://${host}${WS_PATH}`;
    const ws = new WebSocket(url);
    ws.binaryType = "arraybuffer";
    await new Promise<void>((resolve, reject) => {
      ws.onopen = () => resolve();
      ws.onerror = () => reject(new Error(`cannot reach ${url} — is the host running?`));
    });

    const nextText = messageQueue(ws);
    const send = (msg: ControlMsg) => ws.send(JSON.stringify(msg));

    const useToken = stored !== null;
    send({
      type: "hello",
      protocol: PROTOCOL_VERSION,
      client: {
        device_id: deviceId(),
        name: clientName,
        platform: "web",
        app_version: "0.2.0",
        // "cursor": renders the host cursor from the dedicated cursor
        // channel (CursorShape/CursorPos) — never baked into video frames.
        // "clipboard"/"file_drop"/"audio": this build understands those
        // messages/channels; actual use is still permission-gated host-side.
        features: viewerFeatures(),
      },
      auth: useToken ? { method: "token", device_id: stored.deviceId } : { method: "pair" },
      codecs: await supportedCodecs(),
    });

    const ack = await nextText();
    if (ack.type !== "hello_ack") throw protoErr("hello_ack", ack);
    const nonce = b64decode(ack.connection_nonce as string);
    const server = ack.server as { name: string; fingerprint: string };

    if (useToken && stored && stored.hostFingerprint !== server.fingerprint) {
      clearCredentials(host);
      throw new Error(
        "This host's identity changed since you paired (possible impostor). Stored trust was cleared — verify the host and pair again with a PIN.",
      );
    }

    // Ephemeral ECDH — plus a PIN-bound PAKE share when pairing, which makes
    // the recorded transcript useless for offline PIN grinding.
    const keys = await generateHandshakeKeys();
    const pake = useToken ? null : pakeStart(pin!, nonce);
    send({
      type: "pair_start",
      client_pubkey: b64encode(keys.publicRaw),
      ...(pake ? { pake_share: b64encode(pake.share) } : {}),
    });
    const challenge = await nextText();
    if (challenge.type === "auth_err") throw new Error(String(challenge.error));
    if (challenge.type !== "pair_challenge") throw protoErr("pair_challenge", challenge);
    const serverPub = b64decode(challenge.server_pubkey as string);
    const salt = b64decode(challenge.salt as string);
    const shared = await agree(keys, serverPub);

    // No silent downgrade: if we offered a PAKE share the server must answer
    // with one. A missing share means either a pre-PAKE host (update it) or
    // an active attacker stripping the field.
    let pakeSecret: Uint8Array | null = null;
    if (pake) {
      if (typeof challenge.pake_share !== "string") {
        throw new Error(
          "Host did not complete secure PIN pairing (PAKE) — update the host software.",
        );
      }
      pakeSecret = pake.finish(b64decode(challenge.pake_share));
    }
    const sessionKeyRaw = pakeSecret
      ? await sessionKeyBytesPake(shared, pakeSecret, salt, nonce)
      : await sessionKeyBytes(shared, salt, nonce);

    let newlyPaired = false;
    if (useToken && stored) {
      const proof = await tokenProof(b64decode(stored.tokenB64), nonce, keys.publicRaw, serverPub);
      send({ type: "token_proof", proof: b64encode(proof) });
    } else {
      const pairKey = await pairingKeyPake(shared, pakeSecret!, salt, nonce);
      const confirm = new Uint8Array(CONFIRM_CONTEXT.length + nonce.length);
      confirm.set(CONFIRM_CONTEXT, 0);
      confirm.set(nonce, CONFIRM_CONTEXT.length);
      const sealed = await seal(pairKey, confirm, new Uint8Array(0));
      send({ type: "pair_confirm", sealed: b64encode(sealed) });
      const result = await nextText();
      if (result.type !== "pair_result" || !result.ok) {
        throw new Error(`Pairing failed: ${String(result.error ?? "unknown error")}`);
      }
      const token = await open(
        pairKey,
        b64decode(result.sealed_token as string),
        te.encode("token"),
      );
      saveCredentials(host, {
        deviceId: deviceId(),
        tokenB64: b64encode(token),
        hostFingerprint: server.fingerprint,
      });
      newlyPaired = true;
    }

    const authOk = await nextText();
    if (authOk.type === "auth_err") {
      if (useToken) {
        // Token stale (host reset / revoked) — clear so the UI re-pairs.
        clearCredentials(host);
      }
      throw new Error(String(authOk.error));
    }
    if (authOk.type !== "auth_ok") throw protoErr("auth_ok", authOk);

    // Switch to encrypted phase.
    const aesKey = await importAesKey(sessionKeyRaw);
    const sealer = new Sealer(aesKey, DIR_CLIENT_TO_SERVER);
    const opener = new Opener(aesKey, DIR_SERVER_TO_CLIENT);

    const info: SessionInfo = {
      codec: authOk.codec as string,
      mode: authOk.mode as DisplayMode,
      inputAllowed: Boolean(authOk.input_allowed),
      clipboardAllowed: Boolean(authOk.clipboard_allowed),
      audioAvailable: Boolean(authOk.audio_available),
      serverName: server.name,
      fingerprint: server.fingerprint,
      newlyPaired,
    };
    const session = new Session(ws, sealer, info);

    // Serialize decrypt operations to preserve envelope ordering.
    let chain: Promise<void> = Promise.resolve();
    ws.onmessage = (ev: MessageEvent) => {
      if (!(ev.data instanceof ArrayBuffer)) return;
      const data = new Uint8Array(ev.data as ArrayBuffer);
      chain = chain.then(async () => {
        try {
          const { chan, plaintext } = await opener.open(data);
          if (chan === CHANNEL_VIDEO) {
            events.onVideo(parseVideoFrame(plaintext));
          } else if (chan === CHANNEL_CONTROL) {
            events.onControl(JSON.parse(td.decode(plaintext)) as ControlMsg);
          } else if (chan === CHANNEL_AUDIO) {
            events.onAudio(parseAudioFrame(plaintext));
          }
        } catch (e) {
          console.error("envelope error", e);
          events.onClose(`protocol error: ${String(e)}`);
          ws.close();
        }
      });
    };
    ws.onclose = () => events.onClose("connection closed");
    return session;
  }

  /**
   * Sends are chained: envelope counters must hit the wire in seal order or
   * the server's replay protection (counter monotonicity) kills the session.
   * Overlapping awaits on `seal` could otherwise reorder two messages.
   */
  private sendChain: Promise<void> = Promise.resolve();

  send(msg: ControlMsg): Promise<void> {
    this.sendChain = this.sendChain.then(async () => {
      if (this.ws.readyState !== WebSocket.OPEN) return;
      const env = await this.sealer.seal(CHANNEL_CONTROL, te.encode(JSON.stringify(msg)));
      this.ws.send(env);
    });
    return this.sendChain;
  }

  /**
   * Stream one file-drop chunk on channel 4 (only after the host accepted
   * the offer). Shares the send chain so envelope counters stay ordered.
   */
  sendFileChunk(transferId: number, offset: number, data: Uint8Array): Promise<void> {
    this.sendChain = this.sendChain.then(async () => {
      if (this.ws.readyState !== WebSocket.OPEN) return;
      const header = encodeFileChunkHeader(transferId, offset);
      const plaintext = new Uint8Array(header.length + data.length);
      plaintext.set(header, 0);
      plaintext.set(data, header.length);
      const env = await this.sealer.seal(CHANNEL_FILE, plaintext);
      this.ws.send(env);
    });
    return this.sendChain;
  }

  /** Bytes currently queued on the socket (backpressure indicator). */
  get buffered(): number {
    return this.ws.bufferedAmount;
  }

  close(): void {
    this.ws.close();
  }
}

async function supportedCodecs(): Promise<string[]> {
  // JPEG decodes everywhere. H.264 works through either decoder backend:
  // * WebCodecs (secure contexts) with a real avc1 probe — codec-less
  //   Chromium/Electron builds expose VideoDecoder but reject H.264;
  // * MSE + client-side fMP4 remux (works on insecure plain-HTTP LAN
  //   origins, where WebCodecs doesn't exist at all).
  const codecs = ["jpeg"];
  if ((await probeH264Decode()) || caps.mseH264) codecs.unshift("h264");
  // HEVC ahead of H.264 when decodable — better quality per bit, and the
  // host only picks it when it has a hardware HEVC encoder.
  if (await probeHevcDecode()) codecs.unshift("hevc");
  return codecs;
}

function viewerFeatures(): string[] {
  const features = ["cursor", "file_drop"];
  if (typeof navigator !== "undefined" && navigator.clipboard) features.push("clipboard");
  if (typeof AudioDecoder === "function") features.push("audio");
  return features;
}

function protoErr(expected: string, got: ControlMsg): Error {
  return new Error(`protocol error: expected ${expected}, got ${got.type}`);
}

/** Async pull-queue over WebSocket text messages (handshake phase). */
function messageQueue(ws: WebSocket): () => Promise<ControlMsg> {
  const queue: ControlMsg[] = [];
  let waiter: ((m: ControlMsg) => void) | null = null;
  let error: Error | null = null;
  let errWaiter: ((e: Error) => void) | null = null;
  ws.onmessage = (ev: MessageEvent) => {
    if (typeof ev.data !== "string") return;
    const msg = JSON.parse(ev.data) as ControlMsg;
    if (waiter) {
      const w = waiter;
      waiter = null;
      w(msg);
    } else {
      queue.push(msg);
    }
  };
  ws.onclose = () => {
    error = new Error("connection closed during handshake");
    if (errWaiter) errWaiter(error);
  };
  return () =>
    new Promise<ControlMsg>((resolve, reject) => {
      const m = queue.shift();
      if (m) return resolve(m);
      if (error) return reject(error);
      waiter = resolve;
      errWaiter = reject;
    });
}
