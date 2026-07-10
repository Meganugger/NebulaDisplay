# NebulaDisplay
🌌 NebulaDisplay — clean-room virtual monitor &amp; remote display suite: Windows IddCx virtual displays streamed to browser/Android/iOS/desktop viewers over an encrypted, local-only protocol (NDSP). No cloud, no accounts, no telemetry.

## Repository layout

- **[`nebula-mcp/`](nebula-mcp/README.md)** — the **NebulaDisplay MCP server**: a
  production-grade Rust [Model Context Protocol](https://modelcontextprotocol.io)
  server that gives autonomous AI coding agents a full Windows software-
  development environment (filesystem, git/GitHub, terminal & PowerShell,
  process/service control, driver build/sign/install, display & DXGI/DWM
  introspection, benchmarking, crash diagnostics, browser automation and
  network tooling) behind a strict permission model. This is the autonomous
  development backend for building and testing the NebulaDisplay driver and
  viewers. See [`nebula-mcp/README.md`](nebula-mcp/README.md).
