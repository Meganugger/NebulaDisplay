# NebulaDisplay MCP Server

A **production-grade [Model Context Protocol](https://modelcontextprotocol.io)
server** that exposes a comprehensive Windows software-development environment to
autonomous AI coding agents (Claude Code, Cursor, Codex, and any MCP client).

It is the autonomous-engineering backend for [NebulaDisplay](../README.md): the
same machine that builds and tests IddCx virtual-display drivers can be driven
end-to-end by an agent — inspect repos, edit files, compile, test, launch,
debug, benchmark, trace, analyse crashes, automate browsers, build/sign/install
drivers, manage displays, capture logs, run PowerShell, execute Git workflows,
manage services, profile, and inspect the network — with a strict permission
model in front of everything.

- **Language:** Rust (2021 edition, async Tokio).
- **Transport:** MCP over stdio (newline-delimited JSON-RPC 2.0).
- **Tools:** 132 across 15 categories (see [TOOLS.md](TOOLS.md)).
- **Platforms:** cross-platform core; Windows-specific subsystems compile
  everywhere and are active on Windows.

> Status: the cross-platform subsystems (filesystem, terminal, process, git,
> github, network, config, security, telemetry, MCP protocol) are fully
> implemented and tested. Windows-specific subsystems (powershell, services /
> registry / event log, driver toolchain, display, benchmark, diagnostics) are
> implemented as native Win32/DXGI/DWM calls and typed wrappers over the
> standard Windows tooling; they are compiled and checked for the
> `x86_64-pc-windows-msvc`/`-gnu` targets and run on a Windows host. See
> [SECURITY.md](SECURITY.md) and [ROADMAP.md](ROADMAP.md) for external
> limitations (code signing, kernel-mode components).

## Quick start

```bash
# Build
cd nebula-mcp
cargo build --release

# Inspect the tool catalogue
./target/release/nebula-mcp list-tools

# Print a documented default config, then edit the allowlists
./target/release/nebula-mcp print-config > nebula-mcp.toml

# Validate a config
./target/release/nebula-mcp validate-config --config nebula-mcp.toml

# Run the server (speaks MCP over stdio; logs go to stderr)
./target/release/nebula-mcp run --config nebula-mcp.toml --workdir C:\src
```

### Registering with an MCP client

Point your client at the binary with `run` and a config. Example client entry:

```json
{
  "mcpServers": {
    "nebula": {
      "command": "C:\\tools\\nebula-mcp.exe",
      "args": ["run", "--config", "C:\\tools\\nebula-mcp.toml", "--workdir", "C:\\src"]
    }
  }
}
```

`stdout` carries only the MCP protocol; **all logs are written to `stderr`** (and
optionally to rotating files), so the transport is never corrupted.

## Workspace layout

```
nebula-mcp/
├─ crates/
│  ├─ protocol/   # JSON-RPC 2.0 + MCP types + stdio transport
│  ├─ core/       # config, hot reload, security policy, telemetry, metrics,
│  │             #   the Tool trait, registry and per-call context
│  ├─ tools/      # all tool implementations, grouped by category
│  └─ server/     # dispatch engine + `nebula-mcp` binary (CLI)
├─ config/default.toml
└─ *.md           # documentation
```

See [ARCHITECTURE.md](ARCHITECTURE.md) for the design in depth.

## Security first

Nothing is permitted by default. Every tool call runs under a resolved policy:
allowed path globs, a command allowlist, timeouts, a hard runtime ceiling, a
maximum captured-output size, and explicit `allow_elevated` / `allow_network` /
`allow_destructive` gates. There is no unrestricted shell. Read
[SECURITY.md](SECURITY.md) before deploying.

## Documentation

| Document | Contents |
| --- | --- |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Crate design, dispatch, concurrency, cancellation |
| [SECURITY.md](SECURITY.md) | Threat model, permission engine, hardening, limitations |
| [CONFIGURATION.md](CONFIGURATION.md) | Every config field, hot reload, per-tool overrides |
| [TOOLS.md](TOOLS.md) | The complete tool catalogue |
| [API.md](API.md) | MCP methods, request/response shapes, error codes |
| [CHANGELOG.md](CHANGELOG.md) | Release history |
| [ROADMAP.md](ROADMAP.md) | Planned work and known external limitations |

## Development

```bash
cargo test --workspace            # unit + integration tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check

# Cross-check the Windows-native code paths from Linux (requires mingw):
rustup target add x86_64-pc-windows-gnu
CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc \
AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar \
  cargo check --workspace --target x86_64-pc-windows-gnu
```

## License

Licensed under either of Apache-2.0 or MIT at your option.
