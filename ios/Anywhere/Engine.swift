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
    var apps_pending: Bool = false
    var host_version: String = ""
    var has_launcher: Bool = false
    var cursor_captured: Bool = false
    /// The app in front on the Mac ("ELDEN RING", "Safari").
    var front_app: String = ""
    /// A text field on the Mac has the keyboard focus: x, y, width, height
    /// as fractions of the picture. nil when none has.
    var text_field: [Float]?
    /// That field is a password field.
    var text_secure: Bool = false

    /// The focused field as a rectangle of the picture (0…1).
    var textField: CGRect? {
        guard let f = text_field, f.count == 4 else { return nil }
        return CGRect(x: CGFloat(f[0]), y: CGFloat(f[1]), width: CGFloat(f[2]), height: CGFloat(f[3]))
    }

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
    /// The editable layout ("Custom" controls); starts as the game layout.
    var customMap: ControlMap = .game
    /// Open the launcher as soon as the Mac appears, like a console home.
    var launcherOnConnect: Bool = true
    /// Which keyboard opens by itself when a text field on the Mac gets
    /// the focus (owner, 0.6.0: clicking a field left nothing to type with).
    var typingKeyboard: TypingKeyboard = .phone
    /// Phone mode: the Mac adds a screen of this phone's shape with large
    /// text while connected and shows that (owner's choice, 2026-10-20).
    var phoneMode: Bool = true
    /// Play layouts the person changed for one game, by the game's name
    /// (lower case). Games not in here use the built-in layout.
    var gameMaps: [String: ControlMap] = [:]

    /// The control map the chosen profile means.
    var activeMap: ControlMap {
        switch profile {
        case .desktop: return .desktop
        case .game: return .game
        case .eldenRing: return .eldenPlay
        case .custom, .controller: return customMap
        }
    }

    init() {}

    /// Settings saved by an older version may lack newer fields: each one
    /// falls back to its default instead of throwing everything away.
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        resolution = try c.decodeIfPresent(String.self, forKey: .resolution) ?? resolution
        fps = try c.decodeIfPresent(UInt16.self, forKey: .fps) ?? fps
        maxMbps = try c.decodeIfPresent(Float.self, forKey: .maxMbps) ?? maxMbps
        muteMac = try c.decodeIfPresent(Bool.self, forKey: .muteMac) ?? muteMac
        showStats = try c.decodeIfPresent(Bool.self, forKey: .showStats) ?? showStats
        profile = (try? c.decodeIfPresent(InputProfile.self, forKey: .profile)) ?? profile
        pointerSpeed = try c.decodeIfPresent(Double.self, forKey: .pointerSpeed) ?? pointerSpeed
        usageStats = try c.decodeIfPresent(Bool.self, forKey: .usageStats) ?? usageStats
        customMap = (try? c.decodeIfPresent(ControlMap.self, forKey: .customMap)) ?? customMap
        launcherOnConnect = try c.decodeIfPresent(Bool.self, forKey: .launcherOnConnect) ?? launcherOnConnect
        typingKeyboard = (try? c.decodeIfPresent(TypingKeyboard.self, forKey: .typingKeyboard)) ?? typingKeyboard
        phoneMode = try c.decodeIfPresent(Bool.self, forKey: .phoneMode) ?? phoneMode
        gameMaps = (try? c.decodeIfPresent([String: ControlMap].self, forKey: .gameMaps)) ?? gameMaps
    }

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

/// The keyboard that opens when the Mac focuses a text field.
enum TypingKeyboard: String, Codable, CaseIterable {
    /// The iPhone's own keyboard: autocorrect, dictation, other languages.
    case phone
    /// Anywhere's keyboard, driven with the controller's d-pad.
    case controller
    /// Open nothing by itself (X still opens the controller keyboard).
    case off

    var label: String {
        switch self {
        case .phone: return "iPhone keyboard"
        case .controller: return "Controller keyboard"
        case .off: return "Off"
        }
    }
}

/// How the controller is translated while watching the Mac.
enum InputProfile: String, Codable, CaseIterable {
    /// Sticks move the pointer and scroll; buttons click. For using the Mac.
    case desktop
    /// Games, picked by the game in front: its own layout if known or
    /// changed by the person, else WASD + mouse look; play keys while the
    /// game holds the mouse, pointer + click in its menus (owner,
    /// 2026-10-20: "it should adapt to all games on the Mac").
    case game
    /// Your own layout (Settings → Controls).
    case custom
    /// Elden Ring's PC keys; switches between play and menu keys by itself
    /// when the Mac says the game holds the mouse.
    case eldenRing
    /// The controller itself, as-is (works when the host can show a
    /// virtual controller: a PC with ViGEmBus; a Mac needs Apple's
    /// entitlement, which needs the paid developer program).
    case controller

    var label: String {
        switch self {
        case .desktop: return "Desktop"
        case .game: return "Games (auto)"
        case .custom: return "Custom"
        case .eldenRing: return "Elden Ring"
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
    /// Picture zoom (1, 2 or 3×) around the pointer: Mac text is small on
    /// a phone. The Metal view and the pointer overlay both read it.
    @Published var zoom: CGFloat = 1
    /// A keyboard is open: the picture fills the space above it as a wide
    /// band around the focused field (or the pointer), instead of the
    /// whole Mac screen shrunk into that space (owner, 0.6.0: "the window
    /// becomes too small to see what I type"). The Metal view reads it.
    @Published var typingBand = false

    private var seenAppsVersion: UInt32 = 0
    private var appsPendingTicks = 0
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
        let size = Self.phoneDisplaySize()
        if prefs.phoneMode { aa_mobile_set_phone_display(size.0, size.1) } else { aa_mobile_set_phone_display(0, 0) }
        _ = aa_mobile_connect(host, prefs.resolution, prefs.fps, hz, prefs.maxMbps, prefs.muteMac, mock)
        status.state = "connecting"
    }

    /// The game in front, as a key for its saved layout ("" if unknown).
    var gameKey: String { status.front_app.lowercased().trimmingCharacters(in: .whitespaces) }

    /// A short name for the game in front, for labels.
    var gameName: String {
        let n = status.front_app.components(separatedBy: " · ").last ?? ""
        return n.isEmpty ? "this game" : n
    }

    /// The play layout for the game in front: the person's own for it,
    /// else a built-in one for games we know, else the general game layout.
    var gamePlayMap: ControlMap { prefs.gameMaps[gameKey] ?? Self.builtInGameMap(gameKey) }

    /// The built-in play layout for a game (before the person changes it).
    static func builtInGameMap(_ key: String) -> ControlMap {
        key.contains("elden ring") || key.contains("eldenring") ? .eldenPlay : .game
    }

    /// The menu layout for the game in front (pointer is free).
    var gameMenuMap: ControlMap {
        if prefs.gameMaps[gameKey] == nil, gameKey.contains("elden ring") || gameKey.contains("eldenring") {
            return .eldenMenu
        }
        return .gameMenu
    }

    /// The layout in use this moment, following the game's play / menu state.
    var currentMap: ControlMap {
        switch prefs.profile {
        case .game: return status.cursor_captured ? gamePlayMap : gameMenuMap
        case .eldenRing: return status.cursor_captured ? .eldenPlay : .eldenMenu
        default: return prefs.activeMap
        }
    }

    /// The Mac screen Phone mode asks for, in Mac points: 1080 wide
    /// ("Large": Mac text about the size of iPhone text on a 6.1–6.7"
    /// phone) and this phone's shape, held sideways.
    static func phoneDisplaySize() -> (UInt16, UInt16) {
        let b = UIScreen.main.bounds.size
        let long = max(b.width, b.height), short = min(b.width, b.height)
        guard long > 0, short > 0 else { return (1080, 500) }
        let h = Int((1080 * short / long).rounded()) & ~1
        return (1080, UInt16(max(400, h)))
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

    /// Text copied on the phone → the other computer's clipboard.
    func copyText(_ text: String) { aa_mobile_copy_text(text) }

    func setMuteMac(_ on: Bool) {
        prefs.muteMac = on
        aa_mobile_set_mute_host(on)
    }

    // MARK: The launcher

    func requestApps() {
        aa_mobile_request_apps()
    }

    /// Open (or switch to) an app; with `show`, the Mac then shares only
    /// that app, so it fills the phone like a console game.
    func open(_ app: AppEntry, show: Bool = true) { aa_mobile_app_action(app.id, "open", show) }
    func quit(_ app: AppEntry) { aa_mobile_app_action(app.id, "quit", false) }
    func hide(_ app: AppEntry) { aa_mobile_app_action(app.id, "hide", false) }
    /// Back to the whole Mac screen.
    func showDesktop() { aa_mobile_show_desktop() }

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
        // Text the other computer copied → the phone's clipboard.
        let clip = take(aa_mobile_take_clip_text())
        if !clip.isEmpty { UIPasteboard.general.string = clip }
        // A launcher list stuck half-way (a lost page): ask again.
        if status.apps_pending, status.connected {
            appsPendingTicks += 1
            if appsPendingTicks >= 12 { appsPendingTicks = 0; aa_mobile_request_apps() }
        } else {
            appsPendingTicks = 0
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

    nonisolated private static func decode<T: Decodable>(_ type: T.Type, _ json: String) -> T? {
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
