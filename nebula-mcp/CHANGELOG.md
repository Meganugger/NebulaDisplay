# Changelog

All notable changes to the NebulaDisplay MCP server are documented here. The
format is based on [Keep a Changelog](https://keepachangelog.com/) and the
project adheres to [Semantic Versioning](https://semver.org/).

## [0.1.0] - Unreleased

Initial release: a production-grade MCP server exposing a comprehensive Windows
software-development environment to autonomous agents.

### Added

- **MCP protocol** (`nebula-mcp-protocol`): JSON-RPC 2.0 types, MCP 2024-11-05
  domain types, and a newline-delimited JSON stdio transport.
- **Runtime core** (`nebula-mcp-core`):
  - TOML configuration with an atomically-swappable store and debounced hot
    reload.
  - A permission engine (`EffectivePolicy`) covering path allow/deny globs with
    lexical `..` containment, command allowlisting, timeouts, a hard runtime
    ceiling, output caps, and elevation/network/destructive gates.
  - Structured `tracing` telemetry (JSON/pretty to stderr, optional rotating
    files, optional OpenTelemetry OTLP export behind the `otel` feature) and
    lock-free per-tool metrics.
  - The `Tool` trait, a registry honouring category/tool enable switches, and a
    per-call `ToolContext` with unified timeout + cancellation.
- **125 tools** (`nebula-mcp-tools`) across 14 categories:
  - **filesystem** (13): read/write/append/rename/delete/move/copy, content
    search, glob, tree, hash, metadata, permissions, with large-file streaming.
  - **terminal** (6): one-shot run plus persistent interactive sessions.
  - **process** (3): list/info/kill via `sysinfo`.
  - **git** (24): the full everyday command surface plus worktree,
    cherry-pick, revert, reflog, show and apply.
  - **github** (13): clone, a generic authenticated REST tool, and convenience
    tools for PRs, issues, forks, releases, actions, branches, reviews, labels.
  - **network** (10): HTTP(S), DNS, TCP connect, latency sampling, native TLS
    inspection, WebSocket, ping, iperf3, packet capture, QUIC/HTTP3 probe.
  - **powershell** (3): non-interactive, elevated, and remote execution.
  - **windows** (8): services, registry, event log, performance counters,
    scheduled tasks, environment, firewall, network adapters.
  - **driver** (11): MSBuild, inf2cat, signtool, pnputil, devcon, Driver
    Verifier, IddCx diagnostics, display restart, install/uninstall, logs.
  - **display** (9): native QueryDisplayConfig, DXGI adapters/outputs, monitor
    topology, DWM info, present/timing statistics, HDR/advanced-colour
    detection, coordinate-to-monitor mapping, virtual-display enumeration, and
    EnumDisplaySettings mode enumeration.
  - **benchmark** (7): cross-platform system sampling, ffmpeg, PresentMon, WPR,
    wpaexporter, GPUView logger, LatencyMon.
  - **diagnostics** (7): capability probe, WER reports, crash-dump discovery,
    process dump creation, minidump analysis, live stack capture, ETW sessions.
  - **browser** (2): Playwright-driven capture (screenshot/PDF/trace/console/
    network) and a screenshot convenience.
  - **docker** (9): ps, images, build, run, stop, rm, logs, exec, and compose.
- **Progress streaming**: any command-wrapping tool emits MCP
  `notifications/progress` when the client supplies a `progressToken`, so the
  agent gets live heartbeats during long builds/tests/captures.
- **Server** (`nebula-mcp-server`): concurrent, cancellable dispatch with
  bounded parallelism, graceful shutdown, and a CLI (`run`, `list-tools`,
  `print-config`, `validate-config`).
- **Tests**: 81 unit + integration tests, including permission enforcement,
  process timeout/cancellation, and concurrent-dispatch coverage.
- **CI**: Linux (build, test, clippy `-D warnings`, fmt) and Windows (build,
  test) workflows.
- **Docs**: README, ARCHITECTURE, SECURITY, CONFIGURATION, TOOLS, API, ROADMAP.

### Notes

- Windows-only tools compile on all platforms and return a structured
  `platform_unsupported` error off Windows. Native `display` code is compiled
  and type-checked for the Windows target.
- External limitations (code signing, test-signing/reboot prerequisites,
  kernel-mode load requirements, IddCx virtual-display creation) are documented
  in SECURITY.md and ROADMAP.md.
