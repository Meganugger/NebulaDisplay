// Host → viewer file receive (ROADMAP P2.15). The host offers a file (from
// its control panel); nothing is transferred until the person at *this*
// viewer explicitly accepts. Chunks are re-assembled, the announced sha256
// is verified end-to-end, and only a bit-exact file is handed to the
// browser as a download.

import { ControlMsg, b64decode } from "./protocol";
import { sha256Incremental } from "./cryptobox";

/** Must match the host's FILE_CHUNK_BYTES (defense against chunk floods). */
const CHUNK = 256 * 1024;

export interface ReceiveUi {
  /** Ask the user about an offer. Resolve true to accept. */
  confirm(name: string, sizeBytes: number): Promise<boolean>;
  onProgress(received: number, total: number): void;
  /** A verified file is ready — hand it to the browser. */
  onFile(name: string, data: Blob): void;
  onFail(reason: string): void;
}

interface SessionLike {
  send(msg: ControlMsg): Promise<void>;
}

interface ActiveReceive {
  id: string;
  name: string;
  size: number;
  sha256: string;
  parts: Uint8Array[];
  received: number;
  nextSeq: number;
  hasher: ReturnType<typeof sha256Incremental>;
  /** Chunks may only flow after our accept was sent. */
  accepted: boolean;
}

/**
 * Receives one host→viewer transfer at a time. Control messages must be
 * routed into `handleControl` from the session's onControl dispatcher
 * (after the FileSender, which consumes answers to *outgoing* transfers
 * by their own distinct ids).
 */
export class FileReceiver {
  private active: ActiveReceive | null = null;

  constructor(
    private session: SessionLike,
    private ui: ReceiveUi,
  ) {}

  get busy(): boolean {
    return this.active !== null;
  }

  /** Returns true when the message was consumed by the receiver. */
  handleControl(msg: ControlMsg): boolean {
    switch (msg.type) {
      case "file_offer":
        void this.onOffer(
          String(msg.id),
          String(msg.name),
          Number(msg.size_bytes),
          String(msg.sha256),
        );
        return true;
      case "file_chunk":
        if (this.active?.id === msg.id) {
          this.onChunk(Number(msg.seq), String(msg.data));
          return true;
        }
        return false;
      case "file_end":
        if (this.active?.id === msg.id) {
          void this.onEnd();
          return true;
        }
        return false;
      case "file_abort":
        if (this.active?.id === msg.id) {
          this.active = null;
          this.ui.onFail(String(msg.reason ?? "aborted by host"));
          return true;
        }
        return false;
      default:
        return false;
    }
  }

  private async onOffer(id: string, name: string, size: number, sha256: string): Promise<void> {
    const decline = (reason: string) =>
      this.session.send({ type: "file_answer", id, accept: false, reason });
    if (this.active) {
      void decline("another transfer is already running");
      return;
    }
    if (!Number.isSafeInteger(size) || size <= 0) {
      void decline("malformed offer");
      return;
    }
    // Reserve the slot while the user decides — a second offer must not
    // race past the prompt.
    this.active = {
      id,
      name,
      size,
      sha256: sha256.toLowerCase(),
      parts: [],
      received: 0,
      nextSeq: 0,
      hasher: sha256Incremental(),
      accepted: false,
    };
    let accept = false;
    try {
      accept = await this.ui.confirm(name, size);
    } catch {
      accept = false;
    }
    if (this.active?.id !== id) return; // aborted while the prompt was open
    if (!accept) {
      this.active = null;
      void decline("declined on the viewer");
      return;
    }
    this.active.accepted = true;
    void this.session.send({ type: "file_answer", id, accept: true });
  }

  private onChunk(seq: number, dataB64: string): void {
    const a = this.active!;
    const abort = (reason: string) => {
      this.active = null;
      void this.session.send({ type: "file_abort", id: a.id, reason });
      this.ui.onFail(reason);
    };
    if (!a.accepted) return abort("chunk before accept");
    if (seq !== a.nextSeq) return abort(`out-of-order chunk (expected ${a.nextSeq}, got ${seq})`);
    let data: Uint8Array;
    try {
      data = b64decode(dataB64);
    } catch {
      return abort("bad chunk encoding");
    }
    if (data.length === 0 || data.length > CHUNK) return abort("chunk size out of bounds");
    if (a.received + data.length > a.size) return abort("more data than offered");
    a.nextSeq += 1;
    a.received += data.length;
    a.parts.push(data);
    a.hasher.update(data);
    this.ui.onProgress(a.received, a.size);
  }

  private async onEnd(): Promise<void> {
    const a = this.active!;
    this.active = null;
    if (a.received !== a.size) {
      const reason = `size mismatch (${a.received} of ${a.size} bytes)`;
      void this.session.send({ type: "file_abort", id: a.id, reason });
      this.ui.onFail(reason);
      return;
    }
    const digest = [...a.hasher.digest()].map((b) => b.toString(16).padStart(2, "0")).join("");
    if (digest !== a.sha256) {
      const reason = "sha256 mismatch — file corrupted in transit";
      void this.session.send({ type: "file_abort", id: a.id, reason });
      this.ui.onFail(reason);
      return;
    }
    await this.session.send({ type: "file_done", id: a.id });
    this.ui.onFile(a.name, new Blob(a.parts as BlobPart[]));
  }
}
