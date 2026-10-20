// Settings: one column of rows the d-pad walks; A (or left/right) changes
// a value. The same rows the Mac app's Settings has, in phone-sized form,
// plus a "Problems" page with the engine's log for sending back.

import SwiftUI

struct SettingsView: View {
    @EnvironmentObject var engine: Engine
    @EnvironmentObject var pad: Pad
    @EnvironmentObject var router: Router
    @State private var focus = 0
    @State private var token = 0
    @State private var showLog = false

    private struct Row {
        let title: String
        let help: String
        let value: String
        let change: (Int) -> Void
    }

    private var rows: [Row] {
        let p = engine.prefs
        return [
            Row(title: "Picture", help: "1080 is sharp; 720 uses half the data on a weak connection.",
                value: p.resolution == "720" ? "720p" : "1080p") { _ in
                engine.prefs.resolution = p.resolution == "720" ? "1080" : "720"
            },
            Row(title: "Frame rate", help: "75 is the most the phone shows smoothly; 60 saves battery.",
                value: "\(p.fps) fps") { _ in engine.prefs.fps = p.fps == 60 ? 75 : 60 },
            Row(title: "Data cap", help: "The most the Mac may send. Wi-Fi at home: 30–50. Away: 8–15.",
                value: "\(Int(p.maxMbps)) Mbps") { d in
                let steps: [Float] = [5, 8, 15, 30, 50, 80]
                let i = steps.firstIndex { $0 >= p.maxMbps } ?? 3
                engine.prefs.maxMbps = steps[max(0, min(steps.count - 1, i + (d == 0 ? 1 : d)))]
            },
            Row(title: "Controls", help: "Desktop: sticks move the pointer. Game: WASD + mouse look. Controller: as-is.",
                value: p.profile.label) { d in
                let all = InputProfile.allCases
                let i = all.firstIndex(of: p.profile) ?? 0
                engine.prefs.profile = all[(i + (d == 0 ? 1 : d) + all.count) % all.count]
            },
            Row(title: "Typing", help: "Opens by itself when you click a text field on the Mac. iPhone keyboard has autocorrect and dictation (the mic key).",
                value: p.typingKeyboard.label) { d in
                let all = TypingKeyboard.allCases
                let i = all.firstIndex(of: p.typingKeyboard) ?? 0
                engine.prefs.typingKeyboard = all[(i + (d == 0 ? 1 : d) + all.count) % all.count]
            },
            Row(title: "Edit controls", help: "Set what every button and stick does in the Custom layout.",
                value: "Custom layout") { _ in router.screen = .controls },
            Row(title: "Pointer speed", help: "How fast the stick moves the pointer (L3 toggles slow/fast too).",
                value: String(format: "%.2f×", p.pointerSpeed)) { d in
                engine.prefs.pointerSpeed = max(0.5, min(2.5, p.pointerSpeed + (d == 0 ? 0.25 : Double(d) * 0.25)))
            },
            Row(title: "Mac speakers", help: "Mute the Mac's own speakers while connected (sound plays here only).",
                value: p.muteMac ? "Muted" : "Playing") { _ in engine.prefs.muteMac.toggle() },
            Row(title: "Launcher on connect", help: "Open the launcher as soon as the Mac appears, like a console home.",
                value: p.launcherOnConnect ? "On" : "Off") { _ in engine.prefs.launcherOnConnect.toggle() },
            Row(title: "Stats corner", help: "fps, Mbps, delay and loss in the corner of the picture.",
                value: p.showStats ? "On" : "Off") { _ in engine.prefs.showStats.toggle() },
            Row(title: "Problems", help: "What the engine logged, for sending to the project.",
                value: "\(engine.log.count) lines") { _ in showLog.toggle() },
        ]
    }

    var body: some View {
        HStack(spacing: 0) {
            VStack(alignment: .leading, spacing: 8) {
                Text("Settings").font(.system(size: 30, weight: .bold, design: .rounded))
                // 0.6.0 on the phone: ten rows are taller than a phone held
                // sideways, and the list didn't scroll, so the last rows
                // could not be reached and the d-pad focus went off screen.
                // Now it scrolls by touch and follows the focused row.
                ScrollViewReader { proxy in
                    ScrollView(.vertical, showsIndicators: false) {
                        VStack(alignment: .leading, spacing: 8) {
                            ForEach(Array(rows.enumerated()), id: \.offset) { i, r in
                                FocusTile(focused: focus == i, width: 420, height: 50) {
                                    HStack {
                                        Text(r.title).font(.system(size: 16, weight: .semibold))
                                        Spacer()
                                        Text(r.value).font(.system(size: 15)).foregroundStyle(Theme.accent)
                                    }
                                    .padding(.horizontal, 16)
                                }
                                .id(i)
                                .onTapGesture { focus = i; r.change(0) }
                            }
                        }
                        // Room for the focused tile's glow and 1.05x growth.
                        .padding(.horizontal, 14).padding(.vertical, 12)
                    }
                    .onChange(of: focus) { _, f in
                        withAnimation(.easeOut(duration: 0.2)) { proxy.scrollTo(f, anchor: .center) }
                    }
                }
                HintBar(hints: [("A", "Change"), ("◀ ▶", "Adjust"), ("B", "Back")])
            }
            .padding(.horizontal, 18).padding(.top, 20).padding(.bottom, 12)

            VStack(alignment: .leading, spacing: 10) {
                if showLog {
                    Text("Log").font(.system(size: 18, weight: .semibold))
                    // Opens on the newest lines (the ones that matter).
                    ScrollViewReader { proxy in
                        ScrollView {
                            Text(engine.log.suffix(120).joined(separator: "\n"))
                                .font(.system(size: 10, design: .monospaced)).foregroundStyle(Theme.muted)
                                .frame(maxWidth: .infinity, alignment: .leading)
                                .textSelection(.enabled)
                            Color.clear.frame(height: 1).id("end")
                        }
                        .onAppear { proxy.scrollTo("end", anchor: .bottom) }
                        .onChange(of: engine.log.count) { _, _ in proxy.scrollTo("end", anchor: .bottom) }
                    }
                    Text("The full file is viewer.log in the app's Documents folder (Files app → Anywhere).")
                        .font(.system(size: 12)).foregroundStyle(Theme.muted)
                } else {
                    Text(rows[min(focus, rows.count - 1)].help)
                        .font(.system(size: 16)).foregroundStyle(Theme.muted)
                    Spacer()
                    Text("Anywhere \(engine.version) · \(pad.connected ? pad.name : "no controller")")
                        .font(.system(size: 12)).foregroundStyle(Theme.muted.opacity(0.6))
                    if pad.connected {
                        Text(pad.extraButtons.isEmpty ? "No extra buttons reported (L4 / R4 need a job in the Razer Nexus app)" : "Extra buttons: \(pad.extraButtons.joined(separator: ", "))")
                            .font(.system(size: 12)).foregroundStyle(Theme.muted.opacity(0.6))
                    }
                }
            }
            .padding(32)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            .background(Color.black.opacity(0.25))
        }
        .onAppear { pad.mode = .ui; token = pad.push(handle) }
        .onDisappear { pad.pop(token) }
    }

    private func handle(_ ev: NavEvent) {
        switch ev {
        case .up: focus = max(0, focus - 1)
        case .down: focus = min(rows.count - 1, focus + 1)
        case .confirm: rows[focus].change(0)
        case .right: rows[focus].change(1)
        case .left: rows[focus].change(-1)
        case .back, .home: router.screen = .home
        default: break
        }
    }
}
