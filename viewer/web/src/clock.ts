// Clock sync (NTP-style over Ping/Pong) + measured end-to-end latency.

export class ClockSync {
  /** host_clock ≈ client_clock + offsetUs */
  private offsetUs: number | null = null;
  private lastRttMs = 0;
  private bestRttMs = Infinity;

  /** Client timestamp in µs (unix epoch, from performance/now + epoch base). */
  nowUs(): number {
    return (performance.timeOrigin + performance.now()) * 1000;
  }

  /** Handle a pong: t0 = our send time, t1 = server receive/reply time. */
  onPong(t0Us: number, t1Us: number): void {
    const t3 = this.nowUs();
    const rttUs = t3 - t0Us;
    this.lastRttMs = rttUs / 1000;
    // Keep the offset estimated from the *lowest-RTT* sample (least queuing).
    if (this.lastRttMs <= this.bestRttMs + 2) {
      this.bestRttMs = Math.min(this.bestRttMs, this.lastRttMs);
      this.offsetUs = t1Us - (t0Us + rttUs / 2);
    }
  }

  get rttMs(): number {
    return this.lastRttMs;
  }

  /**
   * Measured end-to-end latency for a frame captured at `captureTsUs` (host
   * clock) presented right now. Null until the clock is synced.
   */
  latencyMs(captureTsUs: bigint): number | null {
    if (this.offsetUs === null) return null;
    const captureInClientClock = Number(captureTsUs) - this.offsetUs;
    return (this.nowUs() - captureInClientClock) / 1000;
  }
}
