// The bridge to the Rust engine (crates/aa-mobile): one object the whole
// app talks to. It polls the engine's status a few times a second and
// publishes it, so SwiftUI screens simply read `engine.status`.
//
// In plain words: Swift never touches the network. It asks this object to
// find, pair with and connect to a Mac, and sends it every button press.

import Foundation
import SwiftUI

/// A computer that answered "who's there?".
struct FoundHost: Codable, Identifiable, Hashable {
    var name: String
    var addr: String
    var key: String
    var paired: Bool
    var version: String
    var tailscale: Bool
    var id: String { key.isEmpty ? addr : key }
}

/// A computer we paired with before (from the engine's `paired-hosts.json`).
struct PairedHost: Codable, Identifiable, Hashable {
    var name: String
    var key: String
    var addrs: [String]
    var last_used: UInt64
    var id: String { key }
}

/// What the engine reports about the connection.
struct Status: Codable {
    var state: String = "idle"
    var reason: String = ""
    var paused: String?
    var fps: UInt32 = 0
    var mbps: Float = 0
    var rtt_ms: Float = 0
    var loss_pct: Float = 0
    var codec: String = ""
    var target_fps: UInt16 = 0
    var target_kbps: UInt32 = 0
    var sharing: String = ""
    var notes: [String] = []
    var warning: String?
    var host_mac: Bool = false
    var apps_version: UInt32 = 0
    var full_access: Bool = false

    var connected: Bool { state == "connected" }
    var live: Bool { state == "connecting" || state == "connected" || state == "reconnecting" }
}

/// One tile of the launcher (see `aa_core::apps`).
struct AppEntry: Codable, Identifiable, Hashable {
    var id: String
    var name: String
    var kind: String
    var running: Bool
    var last_used: UInt64
}

/// What the person chose in Settings; saved on the phone.
struct Prefs: Codable {
    var resolution: String = "1080"
    var fps: UInt16 = 60
    var maxMbps: Float = 30
    var muteMac: Bool = false
    var showStats: Bool = true
    var profile: InputProfile = .desktop
    var pointerSpeed: Double = 1.0
    var usageStats: Bool = true

    static func load() -> Prefs {
        if let data = UserDefaults.standard.data(forKey: "prefs"),
           let p = try? JSONDecoder().decode(Prefs.self, from: data) {
            return p
        }
        return Prefs()
    }

    func save() {
        if let data = try? JSONEncoder().encode(self) {
            UserDefaults.standard.set(data, forKey: "prefs")
        }
    }
}

/// How the controller is translated while watching the Mac.
enum InputProfile: String, Codable, CaseIterable {
    /// Sticks move the pointer and scroll; buttons click. For using the Mac.
    case desktop
    /// WASD on the left stick, mouse on the right, FPS-style buttons.
    case game
    /// The controller itself, as-is (works when the host can show a
    /// virtual controller: a PC with ViGEmBus; a Mac needs Apple's
    /// entitlement, which needs the paid developer program).
    case controller

    var label: String {
        switch self {
        case .desktop: return "Desktop"
        case .game: return "Game (keys + mouse)"
        case .controller: return "Controller pass-through"
        }
    }
}

@MainActor
final class Engine: ObservableObject {
    @Published var status = Status()
    @Published var apps: [AppEntry] = []
    @Published var prefs = Prefs.load() {
        didSet { prefs.save() }
    }
    /// Icons by app id, decoded once.
    @Published var icons: [String: UIImage] = [:]
    /// Everything the engine logged, for the Problems screen.
    @Published var log: [String] = []

    /// The controller, so touch and stick can share one pointer.
    weak var pad: Pad?

    private var seenAppsVersion: UInt32 = 0
    private var timer: Timer?
    private var pendingIcons = false

    init() {
        let dir = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0].path
        aa_mobile_init(dir)
        timer = Timer.scheduledTimer(withTimeInterval: 0.25, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.poll() }
        }
    }

    var version: String { String(cString: aa_mobile_version()) }

    // MARK: Finding and pairing

    /// Computers sharing right now. Blocks the engine for about a second,
    /// so it runs off the main thread.
    func discover() async -> [FoundHost] {
        await Task.detached(priority: .userInitiated) {
            Self.decode([FoundHost].self, take(aa_mobile_discover(1200))) ?? []
        }.value
    }

    func paired() -> [PairedHost] {
        Self.decode([PairedHost].self, take(aa_mobile_paired())) ?? []
    }

    func forget(_ host: PairedHost) {
        aa_mobile_forget(host.key)
        objectWillChange.send()
    }

    /// Pair with the six-digit code. Returns nil on success, else why not.
    func pair(addr: String, code: String) async -> String? {
        await Task.detached(priority: .userInitiated) {
            var err: UnsafeMutablePointer<CChar>?
            let rc = aa_mobile_pair(addr, code, &err)
            let text = err.map { take($0) } ?? ""
            return rc == 0 ? nil : (text.isEmpty ? "Pairing failed" : text)
        }.value
    }

    // MARK: The connection

    /// `host`: "key:<hex>" for a paired computer (found wherever it is
    /// now), or an address / name.
    func connect(host: String, mock: Bool = false) {
        apps = []
        icons = [:]
        seenAppsVersion = 0
        UserDefaults.standard.set(host, forKey: "last-host")
        let hz = UInt16(UIScreen.main.maximumFramesPerSecond)
        _ = aa_mobile_connect(host, prefs.resolution, prefs.fps, hz, prefs.maxMbps, prefs.muteMac, mock)
        status.state = "connecting"
    }

    func disconnect() {
        aa_mobile_disconnect()
        status = Status()
        apps = []
    }

    func setMaxMbps(_ v: Float) {
        prefs.maxMbps = v
        aa_mobile_set_max_mbps(v)
    }

    func setMuteMac(_ on: Bool) {
        prefs.muteMac = on
        aa_mobile_set_mute_host(on)
    }

    // MARK: The launcher

    func requestApps() {
        aa_mobile_request_apps()
    }

    func open(_ app: AppEntry) { aa_mobile_app_action(app.id, "open") }
    func quit(_ app: AppEntry) { aa_mobile_app_action(app.id, "quit") }
    func hide(_ app: AppEntry) { aa_mobile_app_action(app.id, "hide") }

    // MARK: Input (thin wrappers so views never see C)

    func mouseAbs(x: Double, y: Double) {
        let cx = UInt16(max(0, min(65535, x * 65535)))
        let cy = UInt16(max(0, min(65535, y * 65535)))
        aa_mobile_mouse_abs(cx, cy)
    }

    func mouseRel(dx: Int, dy: Int) {
        aa_mobile_mouse_rel(Int16(clamping: dx), Int16(clamping: dy))
    }

    func mouseButton(_ button: UInt8, _ pressed: Bool) { aa_mobile_mouse_button(button, pressed) }
    func scroll(dx: Int, dy: Int) { aa_mobile_mouse_scroll(Int16(clamping: dx), Int16(clamping: dy)) }
    func key(_ usage: UInt16, _ pressed: Bool) { aa_mobile_key(usage, pressed) }

    /// A full key tap, with modifiers held around it.
    func tap(_ usage: UInt16, modifiers: [UInt16] = []) {
        for m in modifiers { aa_mobile_key(m, true) }
        aa_mobile_key(usage, true)
        aa_mobile_key(usage, false)
        for m in modifiers.reversed() { aa_mobile_key(m, false) }
    }

    func gamepadAttach(slot: UInt8, playstation: Bool) { aa_mobile_gamepad_attach(slot, playstation) }
    func gamepadDetach(slot: UInt8) { aa_mobile_gamepad_detach(slot) }

    func gamepad(slot: UInt8, buttons: UInt32, lx: Int16, ly: Int16, rx: Int16, ry: Int16, lt: UInt8, rt: UInt8) {
        aa_mobile_gamepad(slot, buttons, lx, ly, rx, ry, lt, rt)
    }

    // MARK: Polling

    private func poll() {
        if let s = Self.decode(Status.self, take(aa_mobile_status())) {
            status = s
            if s.apps_version != seenAppsVersion {
                seenAppsVersion = s.apps_version
                let list = Self.decode([AppEntry].self, take(aa_mobile_apps())) ?? []
                if !list.isEmpty { apps = list }
                loadIcons()
            }
        }
        let text = take(aa_mobile_take_log())
        if !text.isEmpty {
            log.append(contentsOf: text.split(separator: "\n").map(String.init))
            if log.count > 400 { log.removeFirst(log.count - 400) }
        }
    }

    /// Decode any icons that arrived (off the main thread; PNG decoding
    /// of a hundred icons takes a moment).
    private func loadIcons() {
        let ids = apps.map(\.id).filter { icons[$0] == nil }
        if ids.isEmpty { return }
        Task.detached(priority: .utility) {
            var found: [String: UIImage] = [:]
            var buf = [UInt8](repeating: 0, count: 512 * 1024)
            for id in ids {
                let n = buf.withUnsafeMutableBufferPointer { aa_mobile_icon(id, $0.baseAddress, $0.count) }
                if n > 0 && n <= buf.count, let img = UIImage(data: Data(buf[0..<n])) {
                    found[id] = img
                }
            }
            if found.isEmpty { return }
            await MainActor.run { [found] in
                for (k, v) in found { self.icons[k] = v }
            }
        }
    }

    private static func decode<T: Decodable>(_ type: T.Type, _ json: String) -> T? {
        guard let data = json.data(using: .utf8) else { return nil }
        return try? JSONDecoder().decode(type, from: data)
    }
}

/// Take ownership of a Rust string and free it.
func take(_ p: UnsafeMutablePointer<CChar>?) -> String {
    guard let p else { return "" }
    let s = String(cString: p)
    aa_mobile_free(p)
    return s
}
