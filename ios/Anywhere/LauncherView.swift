// The launcher: the Mac's apps and games as rows of big tiles, like a
// console home. Rows: Open now, Recent, Games, Apps, System. D-pad moves,
// A opens (or switches to), X quits a running app, Y hides it, B goes
// back to the picture.

import SwiftUI

struct LauncherView: View {
    @Binding var overlay: Overlay
    @EnvironmentObject var engine: Engine
    @EnvironmentObject var pad: Pad
    @State private var row = 0
    @State private var col = 0
    @State private var token = 0
    @State private var waited = 0

    private struct Row: Identifiable {
        let id: String
        let title: String
        let apps: [AppEntry]
    }

    private var rows: [Row] {
        let all = engine.apps
        var out: [Row] = []
        let running = all.filter(\.running)
        if !running.isEmpty { out.append(Row(id: "running", title: "Open now", apps: running)) }
        let recent = all.filter { $0.last_used > 0 }.sorted { $0.last_used > $1.last_used }
        if !recent.isEmpty { out.append(Row(id: "recent", title: "Recent", apps: Array(recent.prefix(12)))) }
        let games = all.filter { $0.kind == "steam" || $0.kind == "cross_over" }
        if !games.isEmpty { out.append(Row(id: "games", title: "Games", apps: games)) }
        let apps = all.filter { $0.kind == "app" }
        if !apps.isEmpty { out.append(Row(id: "apps", title: "Apps", apps: apps)) }
        let system = all.filter { $0.kind == "system" }
        if !system.isEmpty { out.append(Row(id: "system", title: "System", apps: system)) }
        return out
    }

    private var focused: AppEntry? {
        let r = rows
        guard row < r.count, col < r[row].apps.count else { return nil }
        return r[row].apps[col]
    }

    var body: some View {
        ZStack {
            Color.black.opacity(0.78).ignoresSafeArea().onTapGesture { overlay = .none }
            VStack(alignment: .leading, spacing: 6) {
                HStack {
                    Text("Launcher").font(.system(size: 26, weight: .bold, design: .rounded))
                    Spacer()
                    Text(engine.status.sharing).font(.system(size: 14)).foregroundStyle(Theme.muted)
                }
                .padding(.horizontal, 36).padding(.top, 20)

                if !engine.status.has_launcher {
                    Spacer()
                    Notice(symbol: "arrow.down.circle", title: "Update the Mac's Anywhere",
                           detail: "It runs \(engine.status.host_version.isEmpty ? "an older version" : engine.status.host_version); the launcher needs 0.6.0 or newer. On the Mac: Anywhere → Settings → Update now. Press B to go back to the picture.")
                    Spacer()
                } else if rows.isEmpty {
                    Spacer()
                    Notice(symbol: "hourglass", title: "Asking the Mac…",
                           detail: waited > 6 ? "No list yet. Press B and open the launcher again." : "")
                    Spacer()
                } else {
                    ScrollViewReader { vproxy in
                        ScrollView(.vertical, showsIndicators: false) {
                            VStack(alignment: .leading, spacing: 18) {
                                ForEach(Array(rows.enumerated()), id: \.element.id) { ri, r in
                                    VStack(alignment: .leading, spacing: 8) {
                                        Text(r.title).font(.system(size: 15, weight: .semibold)).foregroundStyle(Theme.muted)
                                            .padding(.horizontal, 36)
                                        ScrollViewReader { hproxy in
                                            ScrollView(.horizontal, showsIndicators: false) {
                                                LazyHStack(spacing: 16) {
                                                    ForEach(Array(r.apps.enumerated()), id: \.element.id) { ci, app in
                                                        tile(app, focused: ri == row && ci == col).id("\(r.id)-\(ci)")
                                                            .onTapGesture { row = ri; col = ci; engine.open(app); overlay = .none }
                                                    }
                                                }
                                                .padding(.horizontal, 36).padding(.vertical, 10)
                                            }
                                            .onChange(of: col) { _, c in
                                                if ri == row { withAnimation { hproxy.scrollTo("\(r.id)-\(c)", anchor: .center) } }
                                            }
                                        }
                                    }
                                    .id(r.id)
                                }
                            }
                            .padding(.bottom, 80)
                        }
                        .onChange(of: row) { _, r in
                            if r < rows.count { withAnimation { vproxy.scrollTo(rows[r].id, anchor: .top) } }
                        }
                    }
                }
                HStack {
                    HintBar(hints: hints)
                    Spacer()
                }
                .padding(.horizontal, 36).padding(.bottom, 16)
            }
        }
        .onAppear {
            token = pad.push(handle)
            Task { @MainActor in await nudge() }
        }
        .onDisappear { pad.pop(token) }
    }

    private var hints: [(String, String)] {
        var h: [(String, String)] = [("A", focused?.running == true ? "Switch to" : "Open"), ("B", "Back")]
        if focused?.running == true { h.append(("X", "Quit")); h.append(("Y", "Hide")) }
        h.append(("L1 R1", "Row"))
        return h
    }

    private func tile(_ app: AppEntry, focused: Bool) -> some View {
        FocusTile(focused: focused, width: 150, height: 150) {
            VStack(spacing: 10) {
                ZStack(alignment: .topTrailing) {
                    if let img = engine.icons[app.id] {
                        Image(uiImage: img).resizable().interpolation(.high).frame(width: 72, height: 72)
                            .clipShape(RoundedRectangle(cornerRadius: 16, style: .continuous))
                    } else {
                        RoundedRectangle(cornerRadius: 16, style: .continuous)
                            .fill(colour(for: app.name))
                            .frame(width: 72, height: 72)
                            .overlay(Text(String(app.name.prefix(1)).uppercased())
                                .font(.system(size: 34, weight: .bold, design: .rounded)).foregroundStyle(.white))
                    }
                    if app.running {
                        Circle().fill(Theme.good).frame(width: 12, height: 12).offset(x: 4, y: -4)
                    }
                }
                Text(app.name).font(.system(size: 14, weight: .semibold)).lineLimit(2).multilineTextAlignment(.center)
                    .frame(height: 36)
            }
            .padding(10)
        }
    }

    /// A steady colour per name, for apps whose icon has not arrived.
    private func colour(for name: String) -> Color {
        var h: UInt32 = 2166136261
        for b in name.utf8 { h = (h ^ UInt32(b)) &* 16777619 }
        return Color(hue: Double(h % 360) / 360, saturation: 0.55, brightness: 0.75)
    }

    private func handle(_ ev: NavEvent) {
        let r = rows
        switch ev {
        case .left: col = max(0, col - 1)
        case .right: if row < r.count { col = min(r[row].apps.count - 1, col + 1) }
        case .up, .l1: row = max(0, row - 1); clampCol()
        case .down, .r1: row = min(r.count - 1, row + 1); clampCol()
        case .confirm:
            if let a = focused { engine.open(a); overlay = .none }
        case .x:
            if let a = focused, a.running { engine.quit(a); refreshSoon() }
        case .y:
            if let a = focused, a.running { engine.hide(a); overlay = .none }
        case .back: overlay = .none
        case .home: overlay = .quickMenu
        default: break
        }
    }

    private func clampCol() {
        let r = rows
        if row < r.count { col = min(col, max(0, r[row].apps.count - 1)) }
    }

    private func refreshSoon() {
        Task { @MainActor in
            try? await Task.sleep(nanoseconds: 1_500_000_000)
            engine.requestApps()
        }
    }

    /// Ask again if no list has come (the first request may have been lost).
    private func nudge() async {
        while overlay == .launcher && engine.apps.isEmpty && waited < 10 {
            try? await Task.sleep(nanoseconds: 2_000_000_000)
            waited += 1
            if engine.apps.isEmpty { engine.requestApps() }
        }
    }
}
