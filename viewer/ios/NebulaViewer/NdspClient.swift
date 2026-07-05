// NebulaDisplay iOS/iPadOS viewer — NDSP v1 client.
//
// URLSessionWebSocketTask-based client, wire-compatible with
// crates/nebula-proto. TLS trust: self-signed host certificates are accepted
// only when their SHA-256 fingerprint matches the pinned value from the QR
// payload / discovery reply (trust-on-first-use otherwise, persisted).

import Foundation
import CryptoKit

public protocol NdspClientDelegate: AnyObject {
    func ndspStateChanged(_ state: NdspClient.State, detail: String?)
    func ndspNeedsPin()
    func ndspVideoPacket(_ packet: NdspClient.VideoPacket)
    func ndspInputPermission(allowed: Bool)
    func ndspError(code: String, message: String)
}

public final class NdspClient: NSObject {
    public enum State { case disconnected, connecting, pairing, ready, streaming }

    public struct VideoPacket {
        public let fullFrame: Bool
        public let frameId: UInt32
        public let x: Int, y: Int, w: Int, h: Int
        public let streamW: Int, streamH: Int
        public let jpeg: Data
    }

    public weak var delegate: NdspClientDelegate?
    public private(set) var inputAllowed = false

    private var task: URLSessionWebSocketTask?
    private var session: URLSession!
    private var hostKey = ""
    private var pinnedFingerprint: String?
    private var profile = "balanced"

    private var deviceId: String {
        let key = "ndsp.device_id"
        if let id = UserDefaults.standard.string(forKey: key) { return id }
        let id = (0..<16).map { _ in String(format: "%02x", UInt8.random(in: 0...255)) }.joined()
        UserDefaults.standard.set(id, forKey: key)
        return id
    }

    public func connect(host: String, port: Int, tls: Bool, fingerprint: String?, profile: String) {
        self.profile = profile
        hostKey = "\(host):\(port)"
        pinnedFingerprint = fingerprint
        delegate?.ndspStateChanged(.connecting, detail: nil)

        session = URLSession(configuration: .default, delegate: self, delegateQueue: nil)
        let scheme = tls ? "wss" : "ws"
        guard let url = URL(string: "\(scheme)://\(host):\(port)/ws") else { return }
        task = session.webSocketTask(with: url)
        task?.resume()
        receiveLoop()
        sendJson([
            "type": "hello",
            "min_version": 1,
            "max_version": 1,
            "client_name": "iOS viewer",
            "device_id": deviceId,
            "capabilities": ["video/mjpeg", "input"],
        ])
    }

    public func disconnect() {
        sendJson(["type": "bye", "resume_token": NSNull()])
        task?.cancel(with: .normalClosure, reason: nil)
        task = nil
        delegate?.ndspStateChanged(.disconnected, detail: nil)
    }

    public func pair(pin: String) {
        sendJson(["type": "pair_request", "pin": pin, "device_name": "iPad/iPhone"])
    }

    public func sendInput(events: [[String: Any]]) {
        guard inputAllowed else { return }
        sendJson(["type": "input", "events": events])
    }

    public func sendFeedback(lastFrame: UInt32, dropped: Int, decodeMs: Double) {
        sendJson([
            "type": "feedback",
            "last_presented_frame": lastFrame,
            "dropped_frames": dropped,
            "decode_ms": decodeMs,
            "queue_depth": 0,
        ])
    }

    // MARK: - Internals

    private func sendJson(_ obj: [String: Any]) {
        guard let data = try? JSONSerialization.data(withJSONObject: obj),
              let text = String(data: data, encoding: .utf8) else { return }
        task?.send(.string(text)) { _ in }
    }

    private func receiveLoop() {
        task?.receive { [weak self] result in
            guard let self else { return }
            switch result {
            case .failure(let error):
                self.delegate?.ndspStateChanged(.disconnected, detail: error.localizedDescription)
            case .success(let message):
                switch message {
                case .string(let text): self.handleControl(text)
                case .data(let data): self.handleBinary(data)
                @unknown default: break
                }
                self.receiveLoop()
            }
        }
    }

    private func token() -> String? {
        UserDefaults.standard.string(forKey: "ndsp.token.\(hostKey)")
    }

    private func handleControl(_ text: String) {
        guard let data = text.data(using: .utf8),
              let msg = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let type = msg["type"] as? String else { return }

        switch type {
        case "hello_ack":
            if (msg["known_device"] as? Bool) == true, let token = token() {
                sendJson(["type": "auth", "token": token])
            } else {
                delegate?.ndspStateChanged(.pairing, detail: nil)
                delegate?.ndspNeedsPin()
            }
        case "pair_ok":
            if let token = msg["token"] as? String {
                UserDefaults.standard.set(token, forKey: "ndsp.token.\(hostKey)")
            }
            startSession()
        case "auth_ok":
            inputAllowed = (msg["input_allowed"] as? Bool) ?? false
            delegate?.ndspInputPermission(allowed: inputAllowed)
            startSession()
        case "session_started":
            delegate?.ndspStateChanged(.streaming, detail: nil)
        case "session_stop":
            delegate?.ndspStateChanged(.ready, detail: msg["reason"] as? String)
        case "input_permission":
            inputAllowed = (msg["allowed"] as? Bool) ?? false
            delegate?.ndspInputPermission(allowed: inputAllowed)
        case "ping":
            if let t = msg["t_micros"] { sendJson(["type": "pong", "t_micros": t]) }
        case "error":
            let code = (msg["code"] as? String) ?? "internal"
            if code == "bad_token" {
                UserDefaults.standard.removeObject(forKey: "ndsp.token.\(hostKey)")
                delegate?.ndspNeedsPin()
            }
            delegate?.ndspError(code: code, message: (msg["message"] as? String) ?? "")
        default:
            break
        }
    }

    private func startSession() {
        delegate?.ndspStateChanged(.ready, detail: nil)
        sendJson([
            "type": "session_start",
            "mode": "mirror",
            "profile": profile,
            "preferred": NSNull(),
            "viewport_width": 2048,
            "viewport_height": 1536,
            "codecs": ["video/mjpeg"],
            "want_audio": false,
        ])
    }

    private func handleBinary(_ data: Data) {
        guard data.count >= 28, data[0] == 0x01, data[1] == 1 else { return }
        func u16(_ offset: Int) -> Int {
            Int(data[offset]) | (Int(data[offset + 1]) << 8)
        }
        func u32(_ offset: Int) -> UInt32 {
            UInt32(data[offset]) | (UInt32(data[offset + 1]) << 8)
                | (UInt32(data[offset + 2]) << 16) | (UInt32(data[offset + 3]) << 24)
        }
        let packet = VideoPacket(
            fullFrame: data[3] & 1 != 0,
            frameId: u32(4),
            x: u16(16), y: u16(18), w: u16(20), h: u16(22),
            streamW: u16(24), streamH: u16(26),
            jpeg: data.subdata(in: 28..<data.count)
        )
        delegate?.ndspVideoPacket(packet)
    }
}

// MARK: - TLS pinning for the host's self-signed certificate

extension NdspClient: URLSessionDelegate {
    public func urlSession(
        _ session: URLSession,
        didReceive challenge: URLAuthenticationChallenge,
        completionHandler: @escaping (URLSession.AuthChallengeDisposition, URLCredential?) -> Void
    ) {
        guard challenge.protectionSpace.authenticationMethod == NSURLAuthenticationMethodServerTrust,
              let trust = challenge.protectionSpace.serverTrust,
              let cert = SecTrustCopyCertificateChain(trust).flatMap({ ($0 as! [SecCertificate]).first })
        else {
            completionHandler(.performDefaultHandling, nil)
            return
        }
        let der = SecCertificateCopyData(cert) as Data
        let digest = SHA256.hash(data: der)
            .map { String(format: "%02X", $0) }
            .joined(separator: ":")

        let tofuKey = "ndsp.certfp.\(hostKey)"
        let expected = pinnedFingerprint ?? UserDefaults.standard.string(forKey: tofuKey)
        if let expected {
            if digest == expected {
                completionHandler(.useCredential, URLCredential(trust: trust))
            } else {
                completionHandler(.cancelAuthenticationChallenge, nil) // spoofed host
            }
        } else {
            // Trust on first use; pin for subsequent connections.
            UserDefaults.standard.set(digest, forKey: tofuKey)
            completionHandler(.useCredential, URLCredential(trust: trust))
        }
    }
}
