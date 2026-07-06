# Release checklist

## Automated gates (must be green)
- [ ] CI: rust-linux, rust-windows, web, browser-e2e all pass on the release commit
- [ ] `cargo audit` (or `cargo deny check advisories`) has no criticals
- [ ] Version bumped in `Cargo.toml` (workspace), `viewer/web/package.json`,
      Android `versionName`, iOS marketing version, `installer/nebuladisplay.iss`

## Manual gates
- [ ] docs/TESTING.md manual matrix executed on real hardware; results recorded
- [ ] Fresh-install walkthrough on a clean Windows VM using only README/docs
- [ ] Driver package built + test-signed + extend mode verified (or release
      notes explicitly say "mirror mode only" for this release)
- [ ] Soak test (4 h) memory/fps stable

## Artifacts
- [ ] `cargo build --release` binaries (x64): nebulad, nebula-tray, nebula-viewer
- [ ] `viewer/web/dist` built with the release version
- [ ] `installer/make-portable.ps1` zip
- [ ] `iscc installer/nebuladisplay.iss` installer
- [ ] (when signed) driver package + `nebula-devnode.exe`
- [ ] SHA-256SUMS for every artifact

## Publish
- [ ] Tag `vX.Y.Z`, GitHub release with: changelog, test-matrix results,
      known issues, upgrade notes (protocol version compatibility statement)
- [ ] Verify download links + hashes from a clean machine
