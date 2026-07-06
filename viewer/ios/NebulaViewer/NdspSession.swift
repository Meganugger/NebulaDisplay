// NDSP session for iOS/iPadOS: URLSessionWebSocketTask transport + CryptoKit
// (P-256 ECDH, HKDF-SHA256, AES-256-GCM). Byte-compatible with the Rust host;
// mirrors shared/client/src/lib.rs. See docs/PROTOCOL.md.

import CryptoKit
import Foundation

public struct NdspVideoFrame {
    public let codec: UInt8 // 0=jpeg 1=h264
    public let keyframe: Bool
    public let seq: UInt32
    public let timestampUs: UInt64
    public let width: UInt16
    public let height: UInt16
    public let payload: Data
}

public protocol NdspSessionDelegate: AnyObject {
    func session(_ session: NdspSession, didReceiveVideo frame: NdspVideoFrame)
    func session(_ session: NdspSession, didReceiveControl message: [String: Any])
    func session(_ session: NdspSession, didCloseWithReason reason: String)
}

public struct NdspCredentials: Codable {
    public let deviceId: String
    public let token: Data
    public let hostFingerprint: String
}

public enum NdspError: LocalizedError {
    case protocolError(String)
    case authFailed(String)
    case hostIdentityChanged

    public var errorDescription: String? {
        switch self {
        case .protocolError(let s): return "protocol error: \(s)"
        case .authFailed(let s): return s
        case .hostIdentityChanged:
            return "Host identity changed since pairing — possible impostor. Verify the host and pair again with a PIN."
        }
    }
}

public final class NdspSession: NSObject {
    public weak var delegate: NdspSessionDelegate?
    public private(set) var codec = "jpeg"
    public private(set) var modeWidth = 0
    public private(set) var modeHeight = 0
    public private(set) var inputAllowed = false
    public private(set) var newCredentials: NdspCredentials?
    public private(set) var serverFingerprint = ""

    private var task: URLSessionWebSocketTask!
    private var sealer: Envelope!
    private var opener: Envelope!

    // MARK: connect

    /// Connect and authenticate. Exactly one of `pin` / `credentials` must be
    /// provided (PIN for first pairing, credentials for reconnects).
    public static func connect(
        host: String, port: UInt16,
        pin: String?, credentials: NdspCredentials?,
        deviceId: String, deviceName: String,
        delegate: NdspSessionDelegate
    ) async throws -> NdspSession {
        precondition(pin != nil || credentials != nil, "need PIN or credentials")
        let session = NdspSession()
        session.delegate = delegate
        let url = URL(string: "ws://\(host):\(port)/ndsp")!
        session.task = URLSession.shared.webSocketTask(with: url)
        session.task.resume()

        func send(_ obj: [String: Any]) async throws {
            let data = try JSONSerialization.data(withJSONObject: obj)
            try await session.task.send(.string(String(data: data, encoding: .utf8)!))
        }
        func recv() async throws -> [String: Any] {
            guard case .string(let text) = try await session.task.receive(),
                  let obj = try JSONSerialization.jsonObject(with: Data(text.utf8)) as? [String: Any]
            else { throw NdspError.protocolError("expected text frame during handshake") }
            if obj["type"] as? String == "auth_err" {
                throw NdspError.authFailed(obj["error"] as? String ?? "authentication rejected")
            }
            return obj
        }

        // 1. hello
        try await send([
            "type": "hello", "protocol": 1,
            "client": [
                "device_id": deviceId, "name": deviceName,
                "platform": "ios", "app_version": "0.2.0",
            ],
            "auth": credentials != nil
                ? ["method": "token", "device_id": deviceId]
                : ["method": "pair"],
            "codecs": ["h264", "jpeg"],
        ])
        let ack = try await recv()
        guard ack["type"] as? String == "hello_ack",
              let nonceB64 = ack["connection_nonce"] as? String,
              let nonce = Data(base64Encoded: nonceB64),
              let server = ack["server"] as? [String: Any],
              let fingerprint = server["fingerprint"] as? String
        else { throw NdspError.protocolError("bad hello_ack") }
        session.serverFingerprint = fingerprint
        if let creds = credentials, creds.hostFingerprint != fingerprint {
            session.task.cancel(with: .normalClosure, reason: nil)
            throw NdspError.hostIdentityChanged
        }

        // 2. ephemeral ECDH (uncompressed SEC1 = CryptoKit x963 representation)
        let ephemeral = P256.KeyAgreement.PrivateKey()
        let clientPub = ephemeral.publicKey.x963Representation
        try await send(["type": "pair_start", "client_pubkey": clientPub.base64EncodedString()])
        let challenge = try await recv()
        guard challenge["type"] as? String == "pair_challenge",
              let serverPub = Data(base64Encoded: challenge["server_pubkey"] as? String ?? ""),
              let salt = Data(base64Encoded: challenge["salt"] as? String ?? "")
        else { throw NdspError.protocolError("bad pair_challenge") }
        let serverKey = try P256.KeyAgreement.PublicKey(x963Representation: serverPub)
        let shared = try ephemeral.sharedSecretFromKeyAgreement(with: serverKey)

        let sessionKey = shared.hkdfDerivedSymmetricKey(
            using: SHA256.self, salt: salt,
            sharedInfo: Data("ndsp-session-v1".utf8) + nonce, outputByteCount: 32)

        // 3. prove PIN or token
        if let creds = credentials {
            var transcript = Data()
            transcript.append(creds.token)
            transcript.append(nonce)
            transcript.append(clientPub)
            transcript.append(serverPub)
            let proof = Data(SHA256.hash(data: transcript))
            try await send(["type": "token_proof", "proof": proof.base64EncodedString()])
        } else {
            let pairKey = shared.hkdfDerivedSymmetricKey(
                using: SHA256.self, salt: salt,
                sharedInfo: Data("ndsp-pair-v1".utf8) + Data(pin!.utf8) + nonce,
                outputByteCount: 32)
            var confirm = Data("ndsp-confirm-v1".utf8)
            confirm.append(nonce)
            let sealed = try AES.GCM.seal(confirm, using: pairKey)
            try await send(["type": "pair_confirm", "sealed": sealed.combined!.base64EncodedString()])
            let result = try await recv()
            guard result["type"] as? String == "pair_result", result["ok"] as? Bool == true,
                  let sealedToken = Data(base64Encoded: result["sealed_token"] as? String ?? "")
            else { throw NdspError.authFailed("pairing failed (wrong PIN?)") }
            let box = try AES.GCM.SealedBox(combined: sealedToken)
            let token = try AES.GCM.open(box, using: pairKey, authenticating: Data("token".utf8))
            session.newCredentials = NdspCredentials(
                deviceId: deviceId, token: token, hostFingerprint: fingerprint)
        }

        // 4. auth_ok → encrypted phase
        let authOk = try await recv()
        guard authOk["type"] as? String == "auth_ok",
              let mode = authOk["mode"] as? [String: Any]
        else { throw NdspError.protocolError("bad auth_ok") }
        session.codec = authOk["codec"] as? String ?? "jpeg"
        session.modeWidth = mode["width"] as? Int ?? 0
        session.modeHeight = mode["height"] as? Int ?? 0
        session.inputAllowed = authOk["input_allowed"] as? Bool ?? false

        session.sealer = Envelope(key: sessionKey, direction: 1)
        session.opener = Envelope(key: sessionKey, direction: 0)
        session.receiveLoop()
        return session
    }

    // MARK: encrypted phase

    /// MainActor so concurrent callers cannot interleave counter allocation
    /// with enqueueing: `seal` (counter++) and the `task.send` call both run
    /// in the synchronous prefix, and URLSessionWebSocketTask preserves the
    /// order of queued sends — the host closes on any counter regression.
    @MainActor
    public func sendControl(_ obj: [String: Any]) async throws {
        let json = try JSONSerialization.data(withJSONObject: obj)
        let envelope = try sealer.seal(channel: 1, plaintext: json)
        try await task.send(.data(envelope))
    }

    public func close() {
        task.cancel(with: .normalClosure, reason: nil)
    }

    private func receiveLoop() {
        Task { [weak self] in
            while let self {
                do {
                    let message = try await self.task.receive()
                    guard case .data(let data) = message else { continue }
                    let (chan, plaintext) = try self.opener.open(envelope: data)
                    switch chan {
                    case 2:
                        if let frame = Self.parseVideoFrame(plaintext) {
                            self.delegate?.session(self, didReceiveVideo: frame)
                        }
                    case 1:
                        if let obj = try JSONSerialization.jsonObject(with: plaintext) as? [String: Any] {
                            if obj["type"] as? String == "input_grant" {
                                self.inputAllowed = obj["allowed"] as? Bool ?? false
                            }
                            self.delegate?.session(self, didReceiveControl: obj)
                        }
                    default: break
                    }
                } catch {
                    self.delegate?.session(self, didCloseWithReason: error.localizedDescription)
                    return
                }
            }
        }
    }

    private static func parseVideoFrame(_ buf: Data) -> NdspVideoFrame? {
        guard buf.count >= 18 else { return nil }
        func be<T: FixedWidthInteger>(_ range: Range<Int>) -> T {
            buf.subdata(in: range).reduce(0) { T($0) << 8 | T($1) }
        }
        return NdspVideoFrame(
            codec: buf[0],
            keyframe: buf[1] & 1 != 0,
            seq: be(2..<6),
            timestampUs: be(6..<14),
            width: be(14..<16),
            height: be(16..<18),
            payload: buf.subdata(in: 18..<buf.count))
    }
}

/// Post-auth envelope framing: [chan u8][counter u64 BE][AES-GCM ct+tag],
/// nonce = [dir, chan, 0, 0, counter BE].
final class Envelope {
    private let key: SymmetricKey
    private let direction: UInt8
    private var sendCounters: [UInt8: UInt64] = [:]
    private var recvExpected: [UInt8: UInt64] = [:]

    init(key: SymmetricKey, direction: UInt8) {
        self.key = key
        self.direction = direction
    }

    private func nonce(dir: UInt8, chan: UInt8, counter: UInt64) -> Data {
        var n = Data([dir, chan, 0, 0])
        withUnsafeBytes(of: counter.bigEndian) { n.append(contentsOf: $0) }
        return n
    }

    func seal(channel: UInt8, plaintext: Data) throws -> Data {
        let counter = sendCounters[channel, default: 0]
        sendCounters[channel] = counter + 1
        let box = try AES.GCM.seal(
            plaintext, using: key,
            nonce: AES.GCM.Nonce(data: nonce(dir: direction, chan: channel, counter: counter)),
            authenticating: Data([channel]))
        var out = Data([channel])
        withUnsafeBytes(of: counter.bigEndian) { out.append(contentsOf: $0) }
        out.append(box.ciphertext)
        out.append(box.tag)
        return out
    }

    func open(envelope: Data) throws -> (UInt8, Data) {
        guard envelope.count >= 25 else { throw NdspError.protocolError("envelope too short") }
        let chan = envelope[0]
        let counter = envelope.subdata(in: 1..<9).reduce(UInt64(0)) { $0 << 8 | UInt64($1) }
        let expected = recvExpected[chan, default: 0]
        guard counter >= expected else { throw NdspError.protocolError("replayed envelope") }
        let body = envelope.subdata(in: 9..<envelope.count)
        let box = try AES.GCM.SealedBox(
            nonce: AES.GCM.Nonce(data: nonce(dir: 0, chan: chan, counter: counter)),
            ciphertext: body.dropLast(16), tag: body.suffix(16))
        let plaintext = try AES.GCM.open(box, using: key, authenticating: Data([chan]))
        recvExpected[chan] = counter + 1
        return (chan, plaintext)
    }
}
