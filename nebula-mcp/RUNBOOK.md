# Autonomous Driver-Development Runbook

This runbook shows how an AI agent drives the NebulaDisplay MCP server through a
full IddCx virtual-display driver cycle — build → sign → install → test →
benchmark → diagnose — using only MCP tool calls. It assumes the
[`config/nebula-driver-dev.toml`](config/nebula-driver-dev.toml) profile and a
Windows host with the WDK/SDK on `PATH`.

All calls are `tools/call` invocations; arguments are shown as JSON. Pass a
`_meta.progressToken` on long calls (build, install, capture) to stream
`notifications/progress`.

## 0. Orient

```jsonc
// What can this host actually do?
{"name":"diagnostics.capabilities","arguments":{}}
// What is my effective policy for a risky tool?
{"name":"security.effective_policy","arguments":{"tool":"driver.install"}}
// Would this path/command be permitted before I try?
{"name":"security.check_path","arguments":{"path":"build/nebula.sys"}}
{"name":"security.check_command","arguments":{"program":"signtool"}}
```

## 1. Inspect the repository

```jsonc
{"name":"git.status","arguments":{"repo":"."}}
{"name":"fs.tree","arguments":{"path":"driver","maxDepth":3}}
{"name":"fs.search","arguments":{"root":"driver","pattern":"EVT_IDD_","glob":"*.cpp"}}
```

## 2. Build

```jsonc
{"name":"driver.build","arguments":{
  "project":"driver/NebulaIdd.vcxproj","configuration":"Release","platform":"x64"}}
```

Fix failures by reading the compiler output, editing with `fs.edit`
(`{"name":"fs.edit","arguments":{"path":"driver/Device.cpp","oldString":"…","newString":"…"}}`),
and rebuilding.

## 3. Create a test certificate and sign

```jsonc
{"name":"driver.create_test_cert","arguments":{
  "subject":"CN=NebulaDisplay Test","store":"CurrentUser",
  "exportPath":"C:/nebula-scratch/nebula-test.cer"}}
// -> returns { "Thumbprint": "AB12…" }

{"name":"driver.inf2cat","arguments":{"driverDir":"build/Release/NebulaIdd"}}

{"name":"driver.signtool","arguments":{
  "file":"build/Release/NebulaIdd/nebulaidd.sys",
  "thumbprint":"AB12…","timestampUrl":"http://timestamp.digicert.com"}}

{"name":"driver.signtool_verify","arguments":{
  "file":"build/Release/NebulaIdd/nebulaidd.sys","kernelPolicy":true}}
```

> External prerequisite: loading a test-signed driver requires test-signing mode
> (`bcdedit /set testsigning on`) and a reboot, which the server cannot perform
> mid-session — see [SECURITY.md](SECURITY.md).

## 4. Install and enable

```jsonc
{"name":"driver.install","arguments":{"inf":"build/Release/NebulaIdd/NebulaIdd.inf"}}
{"name":"driver.iddcx_diagnostics","arguments":{}}
{"name":"display.virtual_displays","arguments":{}}   // confirm the virtual display appears
```

## 5. Verify the display pipeline

```jsonc
{"name":"display.query_config","arguments":{}}
{"name":"display.enum_modes","arguments":{"deviceName":"\\\\.\\DISPLAY2"}}
{"name":"display.duplicate_frame","arguments":{"maxDimension":1280,
  "outputPath":"C:/nebula-scratch/frame.png"}}
{"name":"display.present_stats","arguments":{}}
```

## 6. Benchmark

```jsonc
{"name":"benchmark.presentmon","arguments":{
  "processName":"NebulaHost.exe","timedSecs":15,
  "outputCsv":"C:/nebula-scratch/frames.csv"}}
{"name":"benchmark.system","arguments":{"intervalMs":500}}
```

Poll a long soak with the scheduler instead of holding a call open:

```jsonc
{"name":"scheduler.every","arguments":{
  "program":"PresentMon","args":["-process_name","NebulaHost.exe","-timed","5",
  "-output_file","C:/nebula-scratch/soak.csv","-stop_existing_session"],
  "intervalSecs":60}}
{"name":"scheduler.results","arguments":{"jobId":"…"}}
```

## 7. Diagnose failures / crashes

```jsonc
{"name":"driver.logs","arguments":{"maxEvents":100}}
{"name":"diagnostics.wer_reports","arguments":{}}
{"name":"diagnostics.crash_dumps","arguments":{}}
{"name":"diagnostics.analyze_dump","arguments":{"dump":"C:/…/MEMORY.DMP"}}
// Enable Driver Verifier for the next boot (destructive; takes effect on reboot):
{"name":"driver.verifier","arguments":{"action":"standard","driver":"nebulaidd.sys"}}
```

The `triage_crash_dump` and `diagnose_display_pipeline` MCP prompts
(`prompts/get`) encode these flows for one-shot use.

## 8. Iterate cleanly

```jsonc
{"name":"driver.display_restart","arguments":{}}       // reload the display driver
{"name":"driver.uninstall","arguments":{"publishedName":"oem42.inf"}}
{"name":"git.commit","arguments":{"repo":".","message":"fix: IddCx present path","all":true}}
{"name":"git.push","arguments":{"repo":".","setUpstream":true,"branch":"fix/present-path"}}
{"name":"github.pr_create","arguments":{"owner":"Meganugger","repo":"NebulaDisplay",
  "title":"Fix IddCx present path","head":"fix/present-path","base":"main"}}
```

## Observability while this runs

Start the server with `--metrics-addr 127.0.0.1:9184` and scrape
`http://127.0.0.1:9184/metrics` (Prometheus), or query `diagnostics.metrics`
in-band. Adjust verbosity live with `logging/setLevel`.
