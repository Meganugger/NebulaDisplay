// NebulaDisplay iOS viewer UI: fullscreen stream with touch + Apple Pencil.

import UIKit

final class StreamViewController: UIViewController, NdspClientDelegate {

    private let client = NdspClient()
    private let imageLayer = CALayer()
    private var streamImage: CGContext?
    private var streamW = 0
    private var streamH = 0
    private var lastFrameId: UInt32 = 0
    private var decodeMsEma: Double = 0
    private var feedbackTimer: Timer?

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .black
        imageLayer.contentsGravity = .resizeAspect
        view.layer.addSublayer(imageLayer)

        client.delegate = self
        promptForHost()
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        imageLayer.frame = view.bounds
    }

    // MARK: - Connect / pairing UI

    private func promptForHost() {
        let alert = UIAlertController(
            title: "NebulaDisplay",
            message: "Enter the host address shown on the PC's control panel.",
            preferredStyle: .alert
        )
        alert.addTextField { $0.placeholder = "192.168.1.20:38470" }
        alert.addAction(UIAlertAction(title: "Connect", style: .default) { [weak self] _ in
            guard let self, let text = alert.textFields?.first?.text else { return }
            let parts = text.split(separator: ":")
            let host = String(parts.first ?? "")
            let port = parts.count > 1 ? Int(parts[1]) ?? 38470 : 38470
            self.client.connect(host: host, port: port, tls: true, fingerprint: nil, profile: "drawing")
        })
        present(alert, animated: true)
    }

    func ndspNeedsPin() {
        DispatchQueue.main.async { [weak self] in
            let alert = UIAlertController(
                title: "Pair with host",
                message: "Click “Pair a device” on the host control panel and enter the PIN.",
                preferredStyle: .alert
            )
            alert.addTextField { $0.keyboardType = .numberPad; $0.placeholder = "6-digit PIN" }
            alert.addAction(UIAlertAction(title: "Pair", style: .default) { _ in
                self?.client.pair(pin: alert.textFields?.first?.text ?? "")
            })
            self?.present(alert, animated: true)
        }
    }

    // MARK: - Rendering

    func ndspVideoPacket(_ packet: NdspClient.VideoPacket) {
        let t0 = CFAbsoluteTimeGetCurrent()
        guard let provider = CGDataProvider(data: packet.jpeg as CFData),
              let region = CGImage(
                jpegDataProviderSource: provider, decode: nil,
                shouldInterpolate: false, intent: .defaultIntent
              ) else { return }

        if streamImage == nil || streamW != packet.streamW || streamH != packet.streamH {
            streamW = packet.streamW
            streamH = packet.streamH
            streamImage = CGContext(
                data: nil, width: streamW, height: streamH,
                bitsPerComponent: 8, bytesPerRow: streamW * 4,
                space: CGColorSpaceCreateDeviceRGB(),
                bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
            )
        }
        guard let ctxt = streamImage else { return }
        // CoreGraphics origin is bottom-left; flip the rect's y.
        let rect = CGRect(
            x: packet.x, y: streamH - packet.y - packet.h,
            width: packet.w, height: packet.h
        )
        ctxt.draw(region, in: rect)
        lastFrameId = packet.frameId
        let dt = (CFAbsoluteTimeGetCurrent() - t0) * 1000
        decodeMsEma = decodeMsEma == 0 ? dt : decodeMsEma * 0.9 + dt * 0.1

        if let composed = ctxt.makeImage() {
            DispatchQueue.main.async { [weak self] in
                self?.imageLayer.contents = composed
            }
        }
    }

    // MARK: - Touch & Apple Pencil input

    private func normalized(_ touch: UITouch) -> (Double, Double)? {
        guard streamW > 0 else { return nil }
        let bounds = view.bounds
        let scale = min(bounds.width / CGFloat(streamW), bounds.height / CGFloat(streamH))
        let w = CGFloat(streamW) * scale
        let h = CGFloat(streamH) * scale
        let origin = CGPoint(x: (bounds.width - w) / 2, y: (bounds.height - h) / 2)
        let p = touch.location(in: view)
        let x = (p.x - origin.x) / w
        let y = (p.y - origin.y) / h
        guard (0...1).contains(x), (0...1).contains(y) else { return nil }
        return (Double(x), Double(y))
    }

    private func sendTouches(_ touches: Set<UITouch>, phase: String) {
        var events: [[String: Any]] = []
        for touch in touches {
            guard let (x, y) = normalized(touch) else { continue }
            if touch.type == .pencil {
                events.append([
                    "kind": "stylus", "x": x, "y": y,
                    "pressure": Double(touch.force / max(touch.maximumPossibleForce, 1)),
                    "tilt_x": Double(touch.azimuthUnitVector(in: view).dx) * Double(touch.altitudeAngle),
                    "tilt_y": Double(touch.azimuthUnitVector(in: view).dy) * Double(touch.altitudeAngle),
                    "down": phase != "up" && phase != "cancel",
                    "eraser": false,
                ])
            } else {
                events.append([
                    "kind": "touch",
                    "id": abs(touch.hashValue) % 1_000_000,
                    "phase": phase, "x": x, "y": y,
                    "pressure": NSNull(),
                ])
            }
        }
        if !events.isEmpty { client.sendInput(events: events) }
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) { sendTouches(touches, phase: "down") }
    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) { sendTouches(touches, phase: "move") }
    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) { sendTouches(touches, phase: "up") }
    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) { sendTouches(touches, phase: "cancel") }

    // MARK: - Delegate housekeeping

    func ndspStateChanged(_ state: NdspClient.State, detail: String?) {
        if state == .streaming {
            DispatchQueue.main.async { [weak self] in self?.startFeedback() }
        }
    }

    func ndspInputPermission(allowed: Bool) { /* surface as a banner later */ }

    func ndspError(code: String, message: String) {
        DispatchQueue.main.async { [weak self] in
            if code == "bad_pin" { self?.ndspNeedsPin() }
        }
    }

    private func startFeedback() {
        feedbackTimer?.invalidate()
        feedbackTimer = Timer.scheduledTimer(withTimeInterval: 1.0, repeats: true) { [weak self] _ in
            guard let self else { return }
            self.client.sendFeedback(lastFrame: self.lastFrameId, dropped: 0, decodeMs: self.decodeMsEma)
        }
    }
}
