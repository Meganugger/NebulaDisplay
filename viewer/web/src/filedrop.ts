// File drop (viewer → host): drag a file onto the viewer, the host user
// explicitly accepts it in the control panel, then chunks stream over the
// encrypted file channel and the host verifies the SHA-256 before keeping it.

import { sha256 } from "@noble/hashes/sha2.js";

import { Session } from "./session";

/** Chunk size: large enough for throughput, small enough to interleave. */
const CHUNK = 64 * 1024;
/** Pause streaming while more than this is queued on the socket. */
const MAX_BUFFERED = 4 * 1024 * 1024;

export interface TransferProgress {
  name: string;
  size: number;
  sent: number;
  state: "hashing" | "waiting" | "sending" | "done" | "failed";
  error?: string;
}

type ProgressFn = (p: TransferProgress) => void;

interface PendingTransfer {
  file: File;
  progress: TransferProgress;
  onProgress: ProgressFn;
}

export class FileDropSender {
  private nextId = 1;
  private transfers = new Map<number, PendingTransfer>();

  constructor(private session: Session) {}

  /** Hash the file and offer it. Streaming starts when the host accepts. */
  async offer(file: File, onProgress: ProgressFn): Promise<void> {
    const id = this.nextId++;
    const progress: TransferProgress = {
      name: file.name,
      size: file.size,
      sent: 0,
      state: "hashing",
    };
    this.transfers.set(id, { file, progress, onProgress });
    onProgress(progress);

    const hasher = sha256.create();
    const reader = file.stream().getReader();
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      hasher.update(value);
    }
    const digest = [...hasher.digest()].map((b) => b.toString(16).padStart(2, "0")).join("");

    progress.state = "waiting";
    onProgress(progress);
    await this.session.send({
      type: "file_offer",
      transfer_id: id,
      name: file.name,
      size: file.size,
      sha256: digest,
    });
  }

  /** Wire these three into the session's control-message handler. */
  onAccept(transferId: number): void {
    const t = this.transfers.get(transferId);
    if (!t) return;
    t.progress.state = "sending";
    t.onProgress(t.progress);
    void this.stream(transferId, t).catch((e) => {
      t.progress.state = "failed";
      t.progress.error = String(e);
      t.onProgress(t.progress);
      this.transfers.delete(transferId);
    });
  }

  onReject(transferId: number, reason: string): void {
    const t = this.transfers.get(transferId);
    if (!t) return;
    t.progress.state = "failed";
    t.progress.error = reason;
    t.onProgress(t.progress);
    this.transfers.delete(transferId);
  }

  onDone(transferId: number, ok: boolean, error?: string): void {
    const t = this.transfers.get(transferId);
    if (!t) return;
    t.progress.state = ok ? "done" : "failed";
    if (!ok) t.progress.error = error ?? "transfer failed";
    t.onProgress(t.progress);
    this.transfers.delete(transferId);
  }

  private async stream(id: number, t: PendingTransfer): Promise<void> {
    const reader = t.file.stream().getReader();
    let offset = 0;
    let pending = new Uint8Array(0);
    const sendChunk = async (bytes: Uint8Array) => {
      // Backpressure: never let file data starve input/video on the socket.
      while (this.session.buffered > MAX_BUFFERED) {
        await new Promise((r) => setTimeout(r, 20));
      }
      await this.session.sendFileChunk(id, offset, bytes);
      offset += bytes.length;
      t.progress.sent = offset;
      t.onProgress(t.progress);
    };
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      if (!this.transfers.has(id)) return; // rejected/failed mid-flight
      // Re-slice into fixed chunks regardless of reader granularity.
      let buf = pending.length ? concat(pending, value) : value;
      while (buf.length >= CHUNK) {
        await sendChunk(buf.subarray(0, CHUNK));
        buf = buf.subarray(CHUNK);
      }
      pending = buf.slice();
    }
    if (pending.length) await sendChunk(pending);
  }
}

function concat(a: Uint8Array, b: Uint8Array): Uint8Array {
  const out = new Uint8Array(a.length + b.length);
  out.set(a, 0);
  out.set(b, a.length);
  return out;
}
