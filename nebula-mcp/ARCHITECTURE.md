# Architecture

## Goals

1. **Autonomy** — expose enough capability that an AI engineer rarely needs a
   human: read/write code, build, test, run, debug, benchmark, trace, analyse
   crashes, drive browsers, build/sign/install drivers, manage displays and
   services, and inspect the network.
2. **Safety** — a single, non-bypassable permission choke point in front of
   every tool.
3. **Robustness** — production error handling, cancellation, timeouts, bounded
   resource use, structured telemetry, and no placeholder code.
4. **Portability** — one codebase that compiles everywhere; Windows-specific
   capability is cleanly isolated.

## Crate graph

```
                    ┌──────────────────────┐
                    │  nebula-mcp-protocol │  JSON-RPC 2.0 + MCP types + stdio
                    └──────────┬───────────┘
                               │
                    ┌──────────▼───────────┐
                    │   nebula-mcp-core    │  config, hot reload, security,
                    │                      │  telemetry, metrics, Tool trait,
                    │                      │  registry, ToolContext, errors
                    └──────────┬───────────┘
                               │
                    ┌──────────▼───────────┐
                    │  nebula-mcp-tools    │  132 tools in 15 category modules
                    └──────────┬───────────┘
                               │
                    ┌──────────▼───────────┐
                    │  nebula-mcp-server   │  dispatch engine + `nebula-mcp` bin
                    └──────────────────────┘
```

Dependencies point downward only; the protocol crate has no knowledge of tools,
and tools have no knowledge of dispatch.

### `nebula-mcp-protocol`

Pure data + transport. Models JSON-RPC requests/responses/errors and the MCP
subset needed for a tools server (`initialize`, `tools/list`, `tools/call`,
`ping`, notifications). `FrameReader`/`FrameWriter` implement the
newline-delimited JSON stdio framing; the writer is cloneable and serialises
each frame atomically so concurrent tasks can respond safely. No `unsafe`.

### `nebula-mcp-core`

The runtime foundation:

- **config** — the `Config` document and a `ConfigStore` that holds it behind an
  atomic `Arc` swap, so readers on the hot path are lock-free and hot reload is
  a pointer swap.
- **hotreload** — a debounced file watcher (watches the parent directory to
  catch editor atomic-rename saves) that reloads the store; a bad edit is logged
  and ignored, leaving the last good config live.
- **security** — `EffectivePolicy`, computed per tool by layering the per-tool
  override on the `[security]` baseline. It is the only place that decides path
  access (allow/deny globs + lexical `..` containment), command allowlisting,
  timeouts, the runtime ceiling, output caps, and the elevation/network/
  destructive gates.
- **telemetry** — a `tracing` subscriber writing JSON or pretty logs to
  **stderr** (never stdout) plus optional rotating files, and an optional
  OpenTelemetry OTLP layer behind the `otel` feature.
- **metrics** — lock-free per-tool counters (calls, successes, failures,
  cancellations, durations, output bytes).
- **tool** — the `Tool` trait (async `call`, schema, annotations) and a
  `ToolRegistry` that produces the enabled `tools/list` payload.
- **context** — `ToolContext`, the per-call bundle of policy, working directory,
  cancellation token, metrics and config snapshot, plus `guarded()` which runs a
  future under a timeout + cancellation.

### `nebula-mcp-tools`

Each category is a module exposing `tools()` returning `Arc<dyn Tool>`s. Shared
building blocks live in `common`:

- **exec** — the hardened async process engine: bounded output capture (the
  child is always drained so it can't deadlock on a full pipe), hard timeout and
  cooperative cancellation (killing the process group on Unix), stdin, env and
  cwd control, and precise "tool not found" resolution.
- **session** — a `SessionManager` for persistent interactive processes (shells,
  REPLs) with a bounded output ring buffer, shared across calls via
  `ToolServices`.
- **scheduler** — a `SchedulerManager` for deferred/recurring command execution
  with captured results, also shared via `ToolServices`.
- **args / schema / output** — typed argument extraction with precise errors, a
  small JSON-Schema builder, and result formatting.
- **platform** — `ensure_windows()` gating and a shared PowerShell runner.

Windows-specific tools take one of two forms:

1. **Typed command wrappers** over the real Windows tooling (`sc.exe`,
   `reg.exe`, `pnputil`, `signtool`, `msbuild`, `PresentMon`, `wpr`, PowerShell
   CIM, …). This is the production-standard approach and compiles on every host;
   on non-Windows they return `platform_unsupported`.
2. **Native Win32/DXGI/DWM calls** (the `display` module) using the `windows`
   crate, compiled only for Windows targets (`#[cfg(windows)]`). QueryDisplay
   Config, DXGI adapter/output enumeration, DWM composition/timing, advanced-
   colour/HDR detection, monitor topology, coordinate conversion, display-mode
   enumeration and DXGI Desktop Duplication single-frame capture are all real
   native API calls.

### `nebula-mcp-server`

The dispatch engine and CLI.

## Request lifecycle

```
stdin ─▶ FrameReader ─▶ read loop ─┬─▶ notification? handle inline (cancel, initialized)
                                   │
                                   └─▶ request:
                                        1. register a per-request cancellation
                                           token in `inflight` (synchronously,
                                           before reading the next frame)
                                        2. spawn a task:
                                            a. resolve tool + EffectivePolicy
                                            b. acquire a concurrency permit
                                            c. run under ctx.guarded(max_runtime)
                                            d. record metrics
                                            e. write the response frame
                                        3. remove the token from `inflight`
```

Key properties:

- **Concurrency** — requests are dispatched to a `JoinSet` bounded by a
  `Semaphore` (`server.max_concurrent_calls`), so a slow tool never blocks the
  read loop or other tools.
- **Cancellation** — the per-request token is registered *before* the read loop
  advances, eliminating the race where a `notifications/cancelled` arrives before
  the handler has registered. Cancellation propagates into the process engine,
  which kills the child.
- **Timeouts** — the tool applies any caller-requested timeout internally; the
  dispatcher additionally enforces the policy's hard `max_runtime` ceiling.
- **Graceful shutdown** — on `SIGINT`/`SIGTERM` the root token is cancelled,
  in-flight calls are aborted and drained, then a metrics summary is logged. On a
  plain client EOF, outstanding calls are allowed to finish.
- **Error mapping** — protocol-level problems (unknown method, unknown/disabled
  tool, unparseable params) become JSON-RPC errors; tool execution failures
  become `CallToolResult { isError: true }` with a category-tagged message, so
  the model can read and react to them.

## Why command wrappers for much of Windows

Native bindings for every Windows subsystem (services, registry, ETW, WER,
driver install, Performance Recorder) would be a large `unsafe` surface with
little benefit over the battle-tested first-party CLIs those subsystems ship
with. Wrapping `sc.exe`, `reg.exe`, `pnputil`, `wpr`, `PresentMon`, etc. with
typed arguments and structured JSON output is the robust, maintainable choice
and is exactly how production automation is built. Where a native API is clearly
superior and stable — display configuration and DXGI/DWM introspection — the
`display` module calls it directly.
