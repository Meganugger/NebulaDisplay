# NebulaDisplay Android viewer

Native Kotlin client speaking NDSP v1: OkHttp WebSocket transport, JCA
ECDH/HKDF/AES-GCM pairing (byte-compatible with the Rust host — same flow as
`shared/client`), MediaCodec low-latency H.264 (Annex-B) straight to a
SurfaceView, JPEG fallback, multi-touch forwarding, per-host trust tokens.

> **Note:** this app still pairs via the legacy PIN-HKDF handshake (hosts accept it by default; `allow_legacy_pairing = false` refuses it). Porting NDSP-PAKE v1 here is tracked in docs/ROADMAP.md (P1).


> **Honest status:** complete source, **not compiled in this repo's CI** (no
> Android SDK in the build environment). It follows the same verified protocol
> flow as the Rust/web clients, but expect to fix normal first-build issues.

Build (needs Android Studio or SDK cmdline tools + JDK 17):

```bash
cd viewer/android
gradle wrapper --gradle-version 8.9   # once, generates gradlew
./gradlew assembleDebug
adb install app/build/outputs/apk/debug/app-debug.apk
```

Connect flow: enter `host:port` + PIN from the host panel (or scan the panel
QR with any QR app — it opens the web viewer; the native app gives lower
latency via MediaCodec). Touch control activates once the host grants input
to this device in the control panel.

USB mode (no Wi-Fi): `adb reverse tcp:41800 tcp:41800` on the PC, then
connect to `127.0.0.1:41800` in the app — all traffic flows over the USB
cable. See docs/CONNECTIVITY.md.
