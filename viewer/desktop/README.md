# NebulaDisplay desktop viewer (Windows / macOS / Linux)

A thin [Tauri 2](https://tauri.app) shell around the web viewer
(`viewer/web`), so all NDSP client logic has a single implementation. The
shell adds:

* a native window + fullscreen viewer with no browser chrome,
* a `discover_hosts` command bridging UDP LAN discovery to the UI
  (browsers can't send UDP; the desktop shell can),
* portable, admin-free binaries (`.exe`, `.AppImage`, `.dmg`).

## Build

Prerequisites: Rust, Node, and the Tauri platform deps
(Windows: WebView2 — preinstalled on Win11; Linux: `webkit2gtk-4.1`;
macOS: Xcode CLT).

```bash
cd viewer/web && npm install && npm run build
cd ../desktop/src-tauri
cargo tauri build        # or: cargo tauri dev
```

> Honest status: this crate is a complete, minimal scaffold. It was not
> compiled in this repo's Linux sandbox because `webkit2gtk` system packages
> are unavailable there; it contains no unusual code paths (stock Tauri 2
> layout) and is expected to build wherever Tauri's prerequisites exist.
