# Roadmap

The 0.1.0 release implements the full requested tool surface. This roadmap
tracks deepening, hardening, and the external prerequisites that no software can
work around.

## Near term

- **Streaming tool output** — MCP progress notifications for long-running tools
  (builds, captures) so the agent sees incremental stdout instead of a single
  final blob. The process engine already captures incrementally; this adds a
  `notifications/progress` channel.
- **Resource subscriptions** — `resources/list` and `resources/read` are
  implemented; add `resources/subscribe` + `notifications/resources/updated` for
  live file watching.
- **Structured tool results** — populate MCP `structuredContent` alongside the
  text JSON so schema-aware clients get typed results.
- **Richer git** — worktrees, cherry-pick, reflog, and a safe interactive-rebase
  driver.
- **Per-tool rate limiting** and a global CPU/memory budget guard in addition to
  the existing per-call bounds.

## Windows depth

- **Native services/registry** via the `windows` crate as an alternative to the
  `sc.exe`/`reg.exe` wrappers, for callers that want to avoid spawning.
- **Continuous Desktop Duplication streaming** — single-frame capture
  (`display.duplicate_frame`) is implemented; a streaming/`ffmpeg`-piped variant
  for continuous capture is the next step.
- **ETW consumption in-process** — parse ETW/PresentMon events natively rather
  than shelling out, for lower-latency frame analysis.
- **IddCx virtual-display control** — once NebulaDisplay's IddCx driver exposes
  its control interface, add `display.virtual_create`/`destroy` that speak that
  interface directly (creation is inherently driver-specific).

## Quality & ops

- **Fuzzing** the JSON-RPC frame decoder and argument parsers.
- **Stress/soak** CI job driving thousands of concurrent calls.
- **Signed release binaries** and an MSI/winget package.
- **Metrics export** endpoint (Prometheus) in addition to OTLP traces.

## External limitations (not solvable in software)

These require environment provisioning outside the server's control; see
[SECURITY.md](SECURITY.md):

- **Code signing** needs a real code-signing certificate; production driver
  distribution additionally needs attestation/EV signing and, for broad
  distribution, WHQL.
- **Loading test-signed/unsigned drivers** needs test signing enabled and Secure
  Boot considerations, which require a reboot the server cannot perform
  mid-session.
- **Kernel-mode components** cannot load without the above.
- **Driver Verifier** and display restarts can bugcheck/reset the GPU and take
  effect after reboot.
- **PresentMon/WPR/GPUView/LatencyMon/cdb/procdump**, **ffmpeg/iperf3/curl
  (HTTP3)**, and **Node.js + Playwright** must be installed and on `PATH`; the
  server reports a `tool_not_found` error naming any that are missing.
