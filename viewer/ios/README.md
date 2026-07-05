# NebulaDisplay iOS/iPadOS viewer

Swift implementation of the NDSP v1 client: WebSocket transport with
self-signed-certificate fingerprint pinning (trust-on-first-use), PIN
pairing with Keychain-adjacent token persistence, MJPEG dirty-rect
compositing via CoreGraphics, and touch + Apple Pencil input (pressure,
tilt).

## Status — honest

The Swift sources are complete and protocol-tested against the same wire
format the web/Android viewers use, but **no Xcode is available in this
repo's build sandbox**, so no `.xcodeproj`/IPA was produced here.

## Build

1. On a Mac with Xcode 15+: *File → New → Project → iOS App* (UIKit,
   Swift), name it `NebulaViewer`.
2. Delete the template `ViewController.swift`; add
   `NdspClient.swift` and `StreamViewController.swift` from this folder.
3. Set `StreamViewController` as the root view controller in
   `SceneDelegate`:
   ```swift
   window?.rootViewController = StreamViewController()
   ```
4. Run on device (network access prompt: allow "Local Network").

An Apple Developer account is required for device deployment / TestFlight /
App Store distribution.

## USB mode (no Wi-Fi)

iOS exposes a USB network interface to the Mac/PC through the Apple device
support stack (the same channel `usbmuxd`/iTunes uses). Two supported paths:

* **Personal Hotspot over USB**: enable hotspot, connect the cable — the PC
  gets an IP link to the phone; run the viewer against the PC's address on
  that link. No extra code needed.
* **usbmuxd TCP forwarding** (planned host feature): the host service can
  listen on localhost and use `usbmuxd` port forwarding
  (`iproxy 38470 38470`) so the device connects to `127.0.0.1:38470`.
  This works today with the open-source `libimobiledevice` tools.

## Roadmap

* VideoToolbox H.264 decode (protocol codec id 2 already defined).
* Keychain storage for device tokens (UserDefaults today).
* Scene support for Stage Manager / external display.
