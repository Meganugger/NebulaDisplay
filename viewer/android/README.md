# NebulaDisplay Android viewer

Complete Kotlin implementation of the NDSP v1 protocol: LAN discovery,
PIN pairing with token persistence, TLS with host-fingerprint pinning,
MJPEG dirty-rect rendering on a SurfaceView, and multi-touch + stylus input.

## Build

Requires Android Studio (or the Android SDK + Gradle). This repo's dev
sandbox has no Android SDK, so the APK could not be produced here — the
source is complete and expected to build as-is:

```bash
cd viewer/android
# with Android Studio: open this folder, Run ▶
# CLI (SDK installed, ANDROID_HOME set):
gradle :app:assembleDebug
# → app/build/outputs/apk/debug/app-debug.apk
```

Add a `gradle-wrapper` (`gradle wrapper --gradle-version 8.7`) if you prefer
`./gradlew`.

## USB connection mode (no Wi-Fi)

Android's USB tethering gives the tablet/phone an RNDIS network to the PC —
NDSP runs over it unchanged (it is just an IP link, typically `192.168.42.x`).
Alternatively with adb:

```bash
adb reverse tcp:38470 tcp:38470   # phone connects to 127.0.0.1:38470
```

then enter `127.0.0.1:38470` as the manual address. Both paths avoid Wi-Fi
entirely.

## Roadmap

* Hardware H.264 decode via `MediaCodec` when the host adds the Media
  Foundation encoder (protocol codec id 2 — already wired end to end).
* Foreground service + picture-in-picture.
* Gamepad forwarding (`InputDevice` sources → NDSP gamepad events, protocol
  extension).
