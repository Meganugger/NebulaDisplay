# Release checklist

## Automated gates (must be green)

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace -- -D warnings`
- [ ] `cargo test --workspace` (unit + integration)
- [ ] `cargo check --target x86_64-pc-windows-msvc --no-default-features`
- [ ] `viewer/web`: `npm run typecheck && npm run build`
- [ ] `node tests/browser-smoke.mjs` — screenshots reviewed
- [ ] CI workflow green on the release commit

## Windows build machine

- [ ] `cargo build --release --target x86_64-pc-windows-msvc -p nebula-host`
- [ ] Driver: `msbuild NebulaDisplay.vcxproj /p:Configuration=Release /p:Platform=x64`
- [ ] Driver signed (attestation for public release; test-sign for internal)
- [ ] Tray: `host\tray-ui\build.bat`
- [ ] Installer: `iscc installer\windows\nebuladisplay.iss`
- [ ] Fresh-VM install test: Win10 + Win11 — service starts, firewall rules
      present, panel loads, driver task works, uninstall leaves nothing

## Manual matrix (docs/TESTING.md)

- [ ] Mirror + extend on hardware, 1080p60 stable over Ethernet & 5GHz Wi-Fi
- [ ] Android + iOS + web + desktop viewers pair, stream, input, revoke
- [ ] USB tether / adb reverse path
- [ ] Sleep/wake, monitor unplug, host service restart mid-session
- [ ] Degraded network (netem) — adaptive behavior sane
- [ ] Overnight soak: flat memory, no drops on idle

## Security pass

- [ ] Grep release logs for PINs/tokens/frame bytes (must be none)
- [ ] Trust-store file permissions; TLS key permissions
- [ ] Admin API unreachable from LAN (403)
- [ ] Threat-model residual-risk list still accurate

## Ship

- [ ] Version bump (workspace + web + installer + driver INF DriverVer)
- [ ] CHANGELOG entry
- [ ] Tag `vX.Y.Z`, attach installer + portable host zip + APK
- [ ] Update README screenshots from `tests/artifacts/`
