// SwiftUI app: connect form → fullscreen viewer with touch forwarding.

import SwiftUI

@main
struct NebulaViewerApp: App {
    var body: some Scene {
        WindowGroup { ContentView() }
    }
}

final class ViewerModel: ObservableObject, NdspSessionDelegate {
    @Published var image: CGImage?
    @Published var status = ""
    @Published var connected = false
    @Published var inputAllowed = false

    private var session: NdspSession?
    private let decoder = StreamDecoder()
    private var pingTask: Task<Void, Never>?

    private var deviceId: String {
        if let id = UserDefaults.standard.string(forKey: "ndsp.deviceId") { return id }
        let id = UUID().uuidString
        UserDefaults.standard.set(id, forKey: "ndsp.deviceId")
        return id
    }

    private func creds(for host: String) -> NdspCredentials? {
        guard let data = UserDefaults.standard.data(forKey: "ndsp.creds.\(host)") else { return nil }
        return try? JSONDecoder().decode(NdspCredentials.self, from: data)
    }

    func connect(hostPort: String, pin: String) {
        let parts = hostPort.split(separator: ":")
        let host = String(parts.first ?? "")
        let port = UInt16(parts.count > 1 ? parts[1] : "41800") ?? 41800
        let stored = creds(for: hostPort)
        guard stored != nil || !pin.isEmpty else {
            status = "First connection needs the PIN from the host panel."
            return
        }
        status = "Connecting…"
        decoder.onImage = { [weak self] img, _ in
            DispatchQueue.main.async { self?.image = img }
        }
        Task {
            do {
                let s = try await NdspSession.connect(
                    host: host, port: port,
                    pin: stored == nil ? pin : nil, credentials: stored,
                    deviceId: deviceId, deviceName: UIDevice.current.name,
                    delegate: self)
                if let newCreds = s.newCredentials,
                   let data = try? JSONEncoder().encode(newCreds) {
                    UserDefaults.standard.set(data, forKey: "ndsp.creds.\(hostPort)")
                }
                try await s.sendControl(["type": "set_input_mode", "mode": "direct_touch"])
                await MainActor.run {
                    self.session = s
                    self.inputAllowed = s.inputAllowed
                    self.connected = true
                    self.status = ""
                }
                self.pingTask = Task {
                    while !Task.isCancelled {
                        try? await s.sendControl([
                            "type": "ping",
                            "t0_us": UInt64(Date().timeIntervalSince1970 * 1_000_000),
                        ])
                        try? await Task.sleep(nanoseconds: 1_000_000_000)
                    }
                }
            } catch {
                await MainActor.run { self.status = error.localizedDescription }
            }
        }
    }

    func sendTouch(phase: String, id: Int, location: CGPoint, size: CGSize) {
        guard let session, inputAllowed else { return }
        let event: [String: Any] = [
            "kind": "touch", "id": id, "phase": phase,
            "x": min(max(location.x / max(size.width, 1), 0), 1),
            "y": min(max(location.y / max(size.height, 1), 0), 1),
            "pressure": 1.0,
        ]
        Task { try? await session.sendControl(["type": "input", "events": [event]]) }
    }

    func disconnect() {
        pingTask?.cancel()
        session?.close()
        session = nil
        decoder.invalidate()
        connected = false
    }

    // MARK: NdspSessionDelegate
    func session(_ session: NdspSession, didReceiveVideo frame: NdspVideoFrame) {
        decoder.decode(frame)
    }
    func session(_ session: NdspSession, didReceiveControl message: [String: Any]) {
        if message["type"] as? String == "input_grant" {
            DispatchQueue.main.async { self.inputAllowed = message["allowed"] as? Bool ?? false }
        }
    }
    func session(_ session: NdspSession, didCloseWithReason reason: String) {
        DispatchQueue.main.async {
            self.connected = false
            self.status = reason
        }
    }
}

struct ContentView: View {
    @StateObject private var model = ViewerModel()
    @AppStorage("ndsp.lastHost") private var host = ""
    @State private var pin = ""

    var body: some View {
        if model.connected {
            GeometryReader { geo in
                ZStack {
                    Color.black.ignoresSafeArea()
                    if let img = model.image {
                        Image(decorative: img, scale: 1.0)
                            .resizable()
                            .aspectRatio(contentMode: .fit)
                    } else {
                        ProgressView().tint(.white)
                    }
                }
                .gesture(
                    DragGesture(minimumDistance: 0)
                        .onChanged { v in
                            model.sendTouch(phase: v.translation == .zero ? "start" : "move",
                                            id: 0, location: v.location, size: geo.size)
                        }
                        .onEnded { v in
                            model.sendTouch(phase: "end", id: 0, location: v.location, size: geo.size)
                        }
                )
                .onTapGesture(count: 3) { model.disconnect() } // triple-tap exits
            }
            .statusBarHidden()
        } else {
            VStack(spacing: 16) {
                Text("NebulaDisplay").font(.largeTitle.bold())
                Text("Use this device as an extra screen. Local network only.")
                    .foregroundStyle(.secondary)
                TextField("Host address (192.168.1.20:41800)", text: $host)
                    .textFieldStyle(.roundedBorder)
                    .autocorrectionDisabled()
                    .textInputAutocapitalization(.never)
                SecureField("PIN (first pairing only)", text: $pin)
                    .textFieldStyle(.roundedBorder)
                    .keyboardType(.numberPad)
                Button("Connect") { model.connect(hostPort: host, pin: pin) }
                    .buttonStyle(.borderedProminent)
                Text(model.status).foregroundStyle(.red).font(.footnote)
            }
            .padding(32)
        }
    }
}
