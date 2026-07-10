# NebulaDisplay MCP — Tool Reference

This server exposes **125 MCP tools** across 14 categories. Every tool is
invoked via the standard MCP `tools/call` method and returns a `CallToolResult`
whose text content is (for most tools) a JSON document.

Legend:
- **Cross-platform** tools work on Windows, Linux and macOS.
- **Windows only** tools return a structured `platform_unsupported` error when
  the server runs on a non-Windows host, but are always listed so a Windows
  client can discover them.
- Tools that mutate state check the `allow_destructive` policy; tools that need
  admin rights check `allow_elevated`; tools that reach the network check
  `allow_network`.
- Any command-wrapping tool streams `notifications/progress` when the client
  supplies a `progressToken` on the call.

Input schemas for each tool are advertised via `tools/list` (the `inputSchema`
field) and are the authoritative argument reference.


### `benchmark`

| Tool | Description |
| --- | --- |
| `benchmark.system` | Sample CPU utilisation, per-core load and memory usage over a short interval. Cross-platform. |
| `benchmark.ffmpeg` | Run ffmpeg with the given arguments (encode/decode benchmarking, transcoding). ffmpeg must be allowlisted. Cross-platform. |
| `benchmark.presentmon` | Capture frame timing (frame latency, frame pacing, present mode) with PresentMon to a CSV file. Provide processName or captureAll; the CSV path must be within an allowed root. Windows only. |
| `benchmark.wpr` | Control Windows Performance Recorder: start a profile, or stop and write an ETL trace. Windows only. |
| `benchmark.wpa_export` | Export tables from an ETL trace to CSV using wpaexporter (WPA CLI). Windows only. |
| `benchmark.gpuview` | Drive GPUView's log helper (log.cmd) to start/stop a GPU ETW capture. Provide the full path to log.cmd via 'logCmd' (must be allowlisted). Windows only. |
| `benchmark.latencymon` | Run LatencyMon in CLI mode for DPC/ISR latency measurement. Provide the LatencyMon.exe path via 'exe' (must be allowlisted) and CLI args. Windows only. |

### `browser`

| Tool | Description |
| --- | --- |
| `browser.capture` | Drive Playwright to navigate a URL and optionally capture a screenshot, PDF (Chromium), Playwright trace, console logs and network activity. Requires Node.js + Playwright. |
| `browser.screenshot` | Capture a screenshot of a URL to a file with Playwright. Requires Node.js + Playwright. |

### `diagnostics`

| Tool | Description |
| --- | --- |
| `diagnostics.capabilities` | Report the host platform and availability of external toolchains (git, msbuild, signtool, pnputil, PresentMon, ffmpeg, cdb, ...). Cross-platform. |
| `diagnostics.wer_reports` | List recent Windows Error Reporting (application crash) events as JSON. Windows only. |
| `diagnostics.crash_dumps` | List crash dump (.dmp) files under the local WER CrashDumps directory or a provided directory. Windows only. |
| `diagnostics.create_dump` | Create a full memory dump of a process with procdump (Sysinternals). Windows only. |
| `diagnostics.analyze_dump` | Analyse a crash dump with cdb (!analyze -v) and return the analysis text. cdb must be allowlisted. Windows only. |
| `diagnostics.stack_trace` | Capture stacks of all threads in a running process via cdb (~*k). Windows only. |
| `diagnostics.etw_trace` | Control an ETW trace session with logman: create/start/stop/delete/query. Windows only. |

### `display`

| Tool | Description |
| --- | --- |
| `display.query_config` | Enumerate active display paths (source/target ids, resolution, position, refresh rate) via QueryDisplayConfig. Windows only. |
| `display.dxgi_adapters` | Enumerate DXGI adapters and their outputs (description, dedicated VRAM, desktop coordinates). Windows only. |
| `display.monitors` | Enumerate the monitor topology (device names, work/monitor rects, primary flag). Windows only. |
| `display.dwm_info` | Report DWM composition state and colorization. Windows only. |
| `display.present_stats` | Report DWM composition timing (refresh rate, refresh period, frame counts) as a proxy for present statistics. Windows only. |
| `display.hdr_detection` | Detect advanced colour / HDR capability and current state per display target. Windows only. |
| `display.mouse_to_monitor` | Map a virtual-desktop coordinate to the monitor containing it and to monitor-local coordinates. Windows only. |
| `display.virtual_displays` | List display adapters/devices, flagging indirect (IddCx) virtual displays. Windows only. |
| `display.enum_modes` | Enumerate the supported display modes (resolution, colour depth, refresh) for an adapter via EnumDisplaySettingsEx. Windows only. |

### `docker`

| Tool | Description |
| --- | --- |
| `docker.ps` | List Docker containers (JSON lines). Set all=true to include stopped containers. |
| `docker.images` | List Docker images (JSON lines). |
| `docker.build` | Build a Docker image from a build context directory (within an allowed root). |
| `docker.run` | Run a container from an image. Supports detach, name, env, published ports and a command. |
| `docker.stop` | Stop a running container by name or id. |
| `docker.rm` | Remove a container (optionally forcing). Destructive. |
| `docker.logs` | Fetch a container's logs (last N lines). |
| `docker.exec` | Execute a command inside a running container and capture its output. |
| `docker.compose` | Run docker compose actions (up/down/ps/logs/build) against a compose file (within an allowed root). |

### `driver`

| Tool | Description |
| --- | --- |
| `driver.build` | Build a driver solution/project with MSBuild. Windows only. |
| `driver.inf2cat` | Create a driver catalog (.cat) from an INF directory with inf2cat. Windows only. |
| `driver.signtool` | Sign a driver/catalog/binary with signtool using a certificate store thumbprint. Windows only. |
| `driver.pnputil` | Run pnputil with a bounded action set: enum_drivers, add_driver, delete_driver. Windows only. |
| `driver.devcon` | Run devcon for device management: status, restart, enable, disable. Windows only. |
| `driver.verifier` | Control Driver Verifier: query, standard (enable standard flags for a driver), or reset. Enabling/resetting requires elevation and destructive policy (takes effect after reboot and can cause bugchecks). Windows only. |
| `driver.iddcx_diagnostics` | Report indirect display (IddCx) driver state: display-class PnP devices and their status/problem codes as JSON. Windows only. |
| `driver.display_restart` | Restart display-class PnP devices (Disable/Enable) to reload the display driver. Requires elevation and destructive policy; screens may flicker. Windows only. |
| `driver.install` | Install a driver package from an INF (pnputil /add-driver /install). Requires elevation and destructive policy. Windows only. |
| `driver.uninstall` | Uninstall a published driver package by oemNN.inf name (pnputil /delete-driver /uninstall /force). Requires elevation and destructive policy. Windows only. |
| `driver.logs` | Collect recent PnP/driver-related events (Kernel-PnP configuration log) as JSON. Windows only. |

### `filesystem`

| Tool | Description |
| --- | --- |
| `fs.read` | Read a text file. Supports byte offset and maxBytes for streaming large files in chunks. |
| `fs.write` | Create or overwrite a text file with the given content, creating parent directories. |
| `fs.append` | Append UTF-8 text to a file, creating it if absent. |
| `fs.rename` | Rename or move a file/directory (both paths must be within allowed roots). |
| `fs.delete` | Delete a file or directory. Directory deletion requires recursive=true. Destructive. |
| `fs.move` | Move a file/directory, falling back to copy+delete across filesystems. |
| `fs.copy` | Copy a file or directory tree to a new location. |
| `fs.search` | Search file contents by regular expression under a root directory, returning matches with line numbers. |
| `fs.glob` | List files matching a glob pattern under a root directory. |
| `fs.tree` | Produce a directory tree (names, types, sizes) to a bounded depth. |
| `fs.hash` | Compute the SHA-256 digest and size of a file. |
| `fs.metadata` | Return metadata for a path: type, size, timestamps, and read-only status. |
| `fs.permissions` | Get or set file permissions. On Unix accepts an octal 'mode'; on Windows toggles 'readonly'. |

### `git`

| Tool | Description |
| --- | --- |
| `git.status` | Show the working tree status (porcelain v2 with branch info). |
| `git.diff` | Show changes. Optional 'from'/'to' revisions, 'path' filter, and 'staged' flag. |
| `git.blame` | Show what revision and author last modified each line of a file. |
| `git.log` | Show commit logs. Optional 'maxCount', 'path', and 'revRange'. |
| `git.branch` | List, create or delete branches. Modes: list (default), create, delete. |
| `git.checkout` | Switch branches or restore paths. Provide 'ref' and optional 'create'. |
| `git.merge` | Merge a ref into the current branch. |
| `git.rebase` | Rebase the current branch onto 'ref' (add 'abort' or 'continue' to control an in-progress rebase). |
| `git.stash` | Manage the stash. Modes: push (default), pop, list, drop, apply. |
| `git.tag` | List or create tags. Provide 'name' to create, optional 'message' for an annotated tag. |
| `git.bisect` | Drive a bisect session. Provide 'action' (start/good/bad/reset) and optional 'rev'. |
| `git.commit` | Record staged changes. Provide 'message'; set 'all' to stage tracked changes first. |
| `git.push` | Push to a remote. Optional 'remote' (default origin), 'branch', 'force', 'setUpstream'. |
| `git.pull` | Pull from a remote. Optional 'remote', 'branch', 'rebase'. |
| `git.fetch` | Fetch from a remote. Optional 'remote' (default origin), 'prune', 'all'. |
| `git.reset` | Reset current HEAD. Modes: soft, mixed (default), hard. 'hard' is destructive. |
| `git.clean` | Remove untracked files. Requires 'force'; add 'directories' for -d. Destructive. |
| `git.submodule` | Run a submodule action: status (default), update, init, sync. |
| `git.worktree` | Manage worktrees: list (default), add (needs 'path' and optional 'ref'), remove (needs 'path'). |
| `git.cherry_pick` | Apply the changes of an existing commit. Provide 'commit'. |
| `git.revert` | Revert a commit, creating a new commit. Provide 'commit'; set 'noEdit' to skip the editor. |
| `git.reflog` | Show the reference log. Optional 'maxCount' (default 30). |
| `git.show` | Show an object (commit/tree/blob). Provide 'object' (default HEAD) and optional 'path'. |
| `git.apply` | Apply a patch file to the working tree. Provide 'patch' (path, relative to repo). Set 'check' to only validate. |

### `github`

| Tool | Description |
| --- | --- |
| `github.clone` | Clone a repository into a directory within an allowed root (uses the git CLI). |
| `github.request` | Make an authenticated GitHub REST API request. Covers any endpoint (PRs, issues, actions, releases, reviews, labels, ...). |
| `github.pr_list` | List pull requests for a repository (optional 'state': open/closed/all). |
| `github.pr_create` | Open a pull request. Requires owner, repo, title, head, base. |
| `github.issue_list` | List issues for a repository. |
| `github.issue_create` | Create an issue. Requires owner, repo, title. |
| `github.fork` | Fork a repository into the authenticated account or an org. |
| `github.release_list` | List releases for a repository. |
| `github.release_create` | Create a release from a tag. |
| `github.workflow_runs` | List GitHub Actions workflow runs for a repository. |
| `github.branch_list` | List branches for a repository. |
| `github.review_create` | Submit a review on a pull request (event: APPROVE/REQUEST_CHANGES/COMMENT). |
| `github.labels_list` | List labels defined in a repository. |

### `network`

| Tool | Description |
| --- | --- |
| `net.http_request` | Perform an HTTP/HTTPS request and return status, headers and (capped) body. |
| `net.dns_lookup` | Resolve a hostname to IP addresses. |
| `net.tcp_connect` | Measure TCP connect latency to host:port. |
| `net.latency` | Sample TCP connect latency over multiple attempts and report min/avg/max/jitter. |
| `net.tls_info` | Perform a TLS handshake and report the negotiated protocol and peer certificate details. |
| `net.websocket` | Connect to a WebSocket endpoint, optionally send a text message, and collect replies for a short window. |
| `net.ping` | Ping a host using the system 'ping' utility (must be allowlisted). |
| `net.iperf` | Run an iperf3 client against a server and return JSON throughput results (iperf3 must be allowlisted). |
| `net.packet_capture` | Capture packets to a pcap file using dumpcap/tcpdump. Requires elevation and network policy; the output path must be within an allowed root. |
| `net.quic_probe` | Probe an endpoint for HTTP/3 (QUIC) support using 'curl --http3' (curl must be allowlisted and built with HTTP/3). |

### `powershell`

| Tool | Description |
| --- | --- |
| `powershell.run` | Run a PowerShell command non-interactively (-NoProfile -NonInteractive). Have the script emit JSON (e.g. '... \| ConvertTo-Json') for structured output. Windows only. |
| `powershell.elevated` | Run a PowerShell command elevated (UAC). Output is redirected to a file and returned. Requires allow_elevated. Windows only. |
| `powershell.remote` | Run a PowerShell command on a remote computer via Invoke-Command (WinRM must be configured). Windows only. |

### `process`

| Tool | Description |
| --- | --- |
| `process.list` | List running processes (pid, name, cpu, memory), optionally filtered by a name substring. |
| `process.info` | Return detailed information about a process by pid. |
| `process.kill` | Terminate a process by pid. Destructive; requires allow_destructive. |

### `terminal`

| Tool | Description |
| --- | --- |
| `terminal.run` | Run an allowlisted command to completion, capturing stdout/stderr with a timeout. The program (args[0]) must be permitted by policy; arguments are passed verbatim (no shell). |
| `terminal.session_open` | Open a persistent interactive process (e.g. a shell). Returns a session id to write to and read from across calls. |
| `terminal.session_write` | Write text to an interactive session's stdin. Include a trailing newline to submit a command. |
| `terminal.session_read` | Read and clear buffered output from an interactive session, optionally waiting up to waitMs for new output. |
| `terminal.session_list` | List active interactive sessions. |
| `terminal.session_close` | Terminate an interactive session and free its resources. |

### `windows`

| Tool | Description |
| --- | --- |
| `windows.service` | Query or control a Windows service (query/start/stop/restart) via sc.exe. Windows only. |
| `windows.registry` | Query, set or delete registry keys/values via reg.exe. Delete is destructive. Windows only. |
| `windows.event_log` | Query a Windows event log (e.g. System, Application) and return recent events as JSON. Windows only. |
| `windows.perf_counters` | Sample Windows performance counters (e.g. '\Processor(_Total)\% Processor Time') via Get-Counter. Windows only. |
| `windows.scheduled_tasks` | List or control scheduled tasks (query/run/end) via schtasks.exe. Windows only. |
| `windows.env` | Get or set a Windows environment variable at Process/User/Machine scope. Machine scope requires elevation. Windows only. |
| `windows.firewall` | List Windows firewall rules (optionally filtered by display-name substring) as JSON. Windows only. |
| `windows.network_adapters` | List network adapters (name, status, MAC, link speed) as JSON. Windows only. |
