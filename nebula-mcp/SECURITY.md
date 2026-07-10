# Security

The server is designed to be driven by an autonomous agent, so the permission
model is the most important part of the system. **Nothing is allowed by
default.** Every capability is opt-in through configuration.

## Threat model

- The MCP client (the agent) is *semi-trusted*: it may issue any tool call,
  including mistaken or adversarial ones. The server must never let a tool
  escape its configured sandbox regardless of arguments.
- The host is trusted; the server runs with the privileges of its launching
  user (elevate only when explicitly permitted).
- Secrets on disk (keys, `.env`, SSH/AWS/GPG material) must not be readable via
  the filesystem tools even if they sit inside an allowed root.

## The permission engine

Every `tools/call` resolves an `EffectivePolicy` = the `[security]` baseline
with the tool's `[tools."<name>"]` override layered on top. The policy is the
single choke point for:

### Path access
- `allowed_paths` — glob allowlist. **Empty ⇒ deny all file access.**
- `denied_paths` — glob denylist that always wins, even inside an allowed root.
  Ships denying `**/.ssh/**`, `**/.aws/**`, `**/.gnupg/**`, `**/*.pem`,
  `**/*.key`, `**/id_rsa*`, `**/.env`.
- Paths are lexically normalised (`.`/`..` resolved) **without touching the
  filesystem** before matching. `..` that would escape the filesystem root is
  rejected; the normalised absolute path is what gets matched, so relative
  traversal cannot be used to escape an allowed root.

### Command execution
- `allowed_commands` — allowlist matched by executable basename
  (case-insensitive on Windows, trailing `.exe` stripped). **Empty ⇒ deny all
  execution.**
- There is **no shell interpolation**: `terminal.run` takes a program plus an
  argument array passed verbatim to `CreateProcess`/`execvp`. Arguments are
  never parsed by a shell, eliminating shell-injection.

### Resource bounds
- `default_timeout_secs` — per-call timeout; callers may request a shorter or
  longer value, always clamped to…
- `max_runtime_secs` — an absolute ceiling enforced by the dispatcher.
- `max_output_bytes` — captured stdout/stderr is bounded; the child is still
  fully drained so it cannot deadlock, and truncation is reported.

### Capability gates
- `allow_elevated` — required by `powershell.elevated`, driver install/uninstall,
  Driver Verifier changes, display-driver restart, machine-scope env writes,
  packet capture.
- `allow_network` — required by every tool that reaches off-box (`net.*`,
  `github.*`, `browser.*`).
- `allow_destructive` — required by `fs.delete`, `process.kill`, `git.reset
  --hard`, `git.clean`, registry/service deletion, driver install/uninstall,
  Driver Verifier, display restart.

Per-tool overrides let you, for example, enable `git.push` broadly while keeping
`driver.install` disabled, or raise `terminal.run`'s timeout without loosening
anything else.

## Category and tool switches

- Disable an entire subsystem: `[categories.driver] enabled = false`.
- Disable a single tool: `[tools."fs.delete"] enabled = false`.

Disabled tools are omitted from `tools/list` and rejected at call time.

## Secret handling

- The GitHub tools read a token from `GITHUB_TOKEN`/`GH_TOKEN` and **never echo
  it** in results or logs.
- Logs are structured and go to stderr/files, not stdout; tool arguments are not
  logged verbatim at `info` level.
- The server does not read environment secrets into results.

## Recommended baseline

Start from `config/default.toml` and:

1. Set `allowed_paths` to just your source/build trees.
2. Set `allowed_commands` to just the toolchain you use (`git`, `cargo`,
   `msbuild`, `powershell`, …).
3. Leave `allow_elevated`, `allow_destructive` **off**; enable per-tool only
   where required.
4. Keep `denied_paths` and extend it with any project-specific secret patterns.
5. Run the server as a least-privileged user; only grant admin when a specific
   driver/verifier workflow needs it, and scope it with per-tool overrides.

## Known external limitations

These are properties of Windows itself, not gaps in the server:

- **Driver code signing** requires a valid code-signing certificate (and, for
  production driver distribution, attestation/EV signing and WHQL). The server
  can invoke `signtool`, but cannot mint certificates.
- **Test-signed / unsigned drivers** only load with Secure Boot off and test
  signing on (`bcdedit /set testsigning on`), which requires a reboot the server
  cannot perform mid-session.
- **Driver Verifier** and display-driver restarts can bugcheck or reset the GPU;
  they are gated behind both `allow_elevated` and `allow_destructive` and take
  effect after a reboot.
- **Virtual display creation** requires an IddCx driver and its private control
  interface; the server can enumerate and restart display devices and install
  the driver package, but creating a virtual monitor is the driver's own IOCTL
  surface (part of NebulaDisplay proper).
- **Kernel-mode components** cannot be loaded without the above signing/boot
  prerequisites.
- **ETW/WER/PresentMon/WPR** capture may require elevation and the corresponding
  Windows SDK/tooling to be installed and on `PATH`.
- **QUIC/HTTP3 probing** relies on a `curl` built with HTTP/3.
- **Browser tools** require Node.js and Playwright (with its browser binaries)
  installed on the host.

When a required external tool is missing the server returns a structured
`tool_not_found` error naming it; when a Windows-only tool runs on a non-Windows
host it returns `platform_unsupported`.
