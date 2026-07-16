// Viewer → host file drop (drag a file onto the canvas). The host queues an
// explicit accept/deny decision in its control panel before any bytes flow.

import { generateUuid } from "./caps";
import { ControlMsg } from "./protocol";
import { sha256Incremental } from "./cryptobox";
import { b64encode } from "./protocol";

/** Raw bytes per chunk (must match the host's FILE_CHUNK_BYTES cap). */
const CHUNK = 256 * 1024;
/** Read granularity while hashing/sending. */
const SLICE = 4 * 1024 * 1024;
/** Socket backpressure limit while streaming chunks. */
const MAX_BUFFERED = 4 * 1024 * 1024;

export interface TransferUi {
  onStatus(text: string): void;
  onProgress(sent: number, total: number): void;
  onDone(name: string): void;
  onFail(reason: string): void;
}

interface SessionLike {
  send(msg: ControlMsg): Promise<void>;
  readonly buffered: number;
}

/**
 * Drives one transfer at a time. Control messages for transfers must be
 * routed into `handleControl` from the session's onControl dispatcher.
 */
export class FileSender {
  private active: {
    id: string;
    resolveAnswer?: (accept: boolean, reason?: string) => void;
    resolveEnd?: (ok: boolean, reason?: string) => void;
  } | null = null;

  constructor(private session: SessionLike) {}

  get busy(): boolean {
    return this.active !== null;
  }

  /** Returns true when the message was a transfer message and was consumed. */
  handleControl(msg: ControlMsg): boolean {
    const a = this.active;
    if (!a) return false;
    switch (msg.type) {
      case "file_answer":
        if (msg.id === a.id) {
          a.resolveAnswer?.(Boolean(msg.accept), msg.reason ? String(msg.reason) : undefined);
          return true;
        }
        return false;
      case "file_done":
        if (msg.id === a.id) {
          a.resolveEnd?.(true);
          return true;
        }
        return false;
      case "file_abort":
        if (msg.id === a.id) {
          const reason = String(msg.reason ?? "aborted by host");
          a.resolveAnswer?.(false, reason);
          a.resolveEnd?.(false, reason);
          return true;
        }
        return false;
      default:
        return false;
    }
  }

  async sendFile(file: File, ui: TransferUi): Promise<void> {
    if (this.active) {
      ui.onFail("another transfer is already running");
      return;
    }
    const id = generateUuid();
    this.active = { id };
    try {
      // 1. Hash first — the host verifies end-to-end integrity.
      ui.onStatus(`Preparing ${file.name}…`);
      const hasher = sha256Incremental();
      for (let off = 0; off < file.size; off += SLICE) {
        hasher.update(new Uint8Array(await file.slice(off, off + SLICE).arrayBuffer()));
      }
      const sha = [...hasher.digest()].map((b) => b.toString(16).padStart(2, "0")).join("");

      // 2. Offer and wait for the host-side (panel) decision.
      ui.onStatus(`Waiting for the host to accept ${file.name}… (check the host panel)`);
      const answer = new Promise<{ accept: boolean; reason?: string | undefined }>((resolve) => {
        this.active!.resolveAnswer = (accept, reason) => resolve({ accept, reason });
      });
      await this.session.send({
        type: "file_offer",
        id,
        name: file.name,
        size_bytes: file.size,
        sha256: sha,
      });
      const { accept, reason } = await answer;
      if (!accept) {
        ui.onFail(reason ?? "declined on the host");
        return;
      }

      // 3. Stream chunks with socket backpressure.
      const done = new Promise<{ ok: boolean; reason?: string | undefined }>((resolve) => {
        this.active!.resolveEnd = (ok, reason) => resolve({ ok, reason });
      });
      let seq = 0;
      let sent = 0;
      for (let off = 0; off < file.size; off += SLICE) {
        const slice = new Uint8Array(await file.slice(off, off + SLICE).arrayBuffer());
        for (let c = 0; c < slice.length; c += CHUNK) {
          while (this.session.buffered > MAX_BUFFERED) {
            await new Promise((r) => setTimeout(r, 20));
          }
          const part = slice.subarray(c, Math.min(c + CHUNK, slice.length));
          await this.session.send({
            type: "file_chunk",
            id,
            seq: seq++,
            data: b64encode(part),
          });
          sent += part.length;
          ui.onProgress(sent, file.size);
        }
      }
      await this.session.send({ type: "file_end", id });

      // 4. Wait for the verified completion.
      const end = await done;
      if (end.ok) ui.onDone(file.name);
      else ui.onFail(end.reason ?? "transfer failed");
    } catch (e) {
      ui.onFail((e as Error).message);
      try {
        await this.session.send({ type: "file_abort", id, reason: "viewer error" });
      } catch {
        /* socket already gone */
      }
    } finally {
      this.active = null;
    }
  }
}
