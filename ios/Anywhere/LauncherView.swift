// The launcher, console style: one strip of big tiles along the bottom,
// a hero area with the focused app's icon, name and what A will do, and
// the focused icon blurred behind everything. Tabs (Open now · Recent ·
// Games · Apps · System) switch with L1 / R1 or up / down. Left / right
// move along the strip; A opens the app and the Mac then shows only that
// app, so it fills the phone like a game. The first tile of every tab is
// Desktop: the whole Mac screen.

import SwiftUI

struct LauncherView: View {
    @Binding var overlay: Overlay
    @EnvironmentObject var engine: Engine
    @EnvironmentObject var pad: Pad
    @State private var tab = 0
    @State private var col = 0
    @State private var token = 0
    @State private var waited = 0

    /// The "Desktop" tile, present in every tab.
    static let desktop = AppEntry(id: "desktop", name: "Desktop", kind: "desktop", running: true, last_used: 0)

    private struct Tab: Identifiable {
        let id: String
        let title: String
        let apps: [AppEntry]
    }

    private var tabs: [Tab] {
        let all = engine.apps
        var out: [Tab] = []
        let running = all.filter(\.running)
        if !running.isEmpty { out.append(Tab(id: "running", title: "Open now", apps: running)) }
        let recent = all.filter { $0.last_used > 0 }.sorted { $0.last_used > $1.last_used }
        if !recent.isEmpty { out.append(Tab(id: "recent", title: "Recent", apps: Array(recent.prefix(12)))) }
        let games = all.filter { $0.kind == "steam" || $0.kind == "cross_over" }
        if !games.isEmpty { out.append(Tab(id: "games", title: "Games", apps: games)) }
        let apps = all.filter { $0.kind == "app" }
        if !apps.isEmpty { out.append(Tab(id: "apps", title: "Apps", apps: apps)) }
        let system = all.filter { $0.kind == "system" }
        if !system.isEmpty { out.append(Tab(id: "system", title: "System", apps: system)) }
        if out.isEmpty { out.append(Tab(id: "mac", title: "Mac", apps: [])) }
        return out.map { Tab(id: $0.id, title: $0.title, apps: [Self.desktop] + $0.apps) }
    }

    private var current: Tab { tabs[min(tab, tabs.count - 1)] }
    private var focused: AppEntry? { col < current.apps.count ? current.apps[col] : nil }
    private var isDesktop: Bool { focused?.id == Self.desktop.id }

    var body: some View {
        ZStack {
            background
            VStack(alignment: .leading, spacing: 0) {
                topBar.padding(.horizontal, 40).padding(.top, 18)
                Spacer(minLength: 8)
                if !engine.status.has_launcher {
                    Notice(symbol: "arrow.down.circle", title: "Update the Mac's Anywhere",
                           detail: "It runs \(engine.status.host_version.isEmpty ? "an older version" : engine.status.host_version); the launcher needs 0.6.0 or newer. On the Mac: Anywhere → Settings → Update now. Press B for the picture.")
                        .frame(maxWidth: .infinity)
                } else if engine.apps.isEmpty && waited > 6 {
                    Notice(symbol: "hourglass", title: "No list from the Mac yet", detail: "Press B, then open the launcher again.")
                        .frame(maxWidth: .infinity)
                } else {
                    hero.padding(.horizontal, 40)
                }
                Spacer(minLength: 8)
                strip
                HStack {
                    HintBar(hints: hints)
                    Spacer()
                    if !engine.status.full_access, engine.status.has_launcher {
                        Text("For one app at a time: on the Mac, Share → this phone → Full access")
                            .font(.system(size: 12)).foregroundStyle(Theme.warn)
                    }
                }
                .padding(.horizontal, 40).padding(.bottom, 14)
            }
        }
        .onAppear {
            token = pad.push(handle)
            Task { @MainActor in await nudge() }
        }
        .onDisappear { pad.pop(token) }
        .onChange(of: tab) { _, _ in col = min(col, max(0, current.apps.count - 1)) }
    }

    // MARK: Pieces

    @ViewBuilder private var background: some View {
        Theme.bg.ignoresSafeArea()
        if let f = focused, let img = engine.icons[f.id] {
            Image(uiImage: img).resizable().scaledToFill()
                .frame(maxWidth: .infinity, maxHeight: .infinity).clipped()
                .blur(radius: 80).opacity(0.45).ignoresSafeArea()
                .animation(.easeInOut(duration: 0.35), value: f.id)
        } else if let f = focused, !isDesktop {
            colour(for: f.name).opacity(0.35).ignoresSafeArea()
        }
        LinearGradient(colors: [.black.opacity(0.2), .black.opacity(0.75)], startPoint: .top, endPoint: .bottom).ignoresSafeArea()
    }

    private var topBar: some View {
        HStack(alignment: .firstTextBaseline, spacing: 18) {
            Text(engine.status.sharing.isEmpty ? "Mac" : engine.status.sharing)
                .font(.system(size: 17, weight: .semibold)).foregroundStyle(Theme.muted).lineLimit(1)
            Spacer()
            ForEach(Array(tabs.enumerated()), id: \.element.id) { i, t in
                Text(t.title)
                    .font(.system(size: 17, weight: i == tab ? .bold : .medium, design: .rounded))
                    .foregroundStyle(i == tab ? Theme.text : Theme.muted)
                    .padding(.vertical, 4)
                    .overlay(alignment: .bottom) {
                        Capsule().fill(i == tab ? Theme.text : .clear).frame(height: 3).offset(y: 4)
                    }
                    .onTapGesture { tab = i; col = 0 }
            }
        }
    }

    private var hero: some View {
        HStack(spacing: 28) {
            bigIcon.frame(width: 150, height: 150)
            VStack(alignment: .leading, spacing: 8) {
                Text(focused?.name ?? "").font(.system(size: 40, weight: .bold, design: .rounded)).lineLimit(2)
                Text(subtitle).font(.system(size: 16)).foregroundStyle(Theme.muted)
                HStack(spacing: 10) {
                    Text("A").font(.system(size: 13, weight: .bold, design: .rounded))
                        .frame(width: 26, height: 26).background(Circle().fill(Color.white)).foregroundStyle(.black)
                    Text(primaryAction).font(.system(size: 17, weight: .semibold))
                }
                .padding(.top, 8)
            }
            Spacer()
        }
    }

    @ViewBuilder private var bigIcon: some View {
        if isDesktop {
            RoundedRectangle(cornerRadius: 34, style: .continuous).fill(Color.white.opacity(0.12))
                .overlay(Image(systemName: "macwindow").font(.system(size: 64, weight: .light)))
        } else if let f = focused, let img = engine.icons[f.id] {
            Image(uiImage: img).resizable().interpolation(.high)
                .clipShape(RoundedRectangle(cornerRadius: 34, style: .continuous))
                .shadow(color: .black.opacity(0.5), radius: 24, y: 12)
        } else if let f = focused {
            RoundedRectangle(cornerRadius: 34, style: .continuous).fill(colour(for: f.name))
                .overlay(Text(String(f.name.prefix(1)).uppercased()).font(.system(size: 72, weight: .bold, design: .rounded)))
        }
    }

    private var subtitle: String {
        guard let f = focused else { return "" }
        if isDesktop { return "The whole Mac screen" }
        var parts: [String] = []
        switch f.kind {
        case "steam": parts.append("Steam game")
        case "cross_over": parts.append("Windows program (CrossOver)")
        case "system": parts.append("System")
        default: parts.append("App")
        }
        if f.running { parts.append("open now") }
        return parts.joined(separator: " · ")
    }

    private var primaryAction: String {
        guard let f = focused else { return "" }
        if isDesktop { return engine.status.sharing == "Whole screen" || engine.status.sharing.isEmpty ? "Back to the Mac" : "Show the whole Mac" }
        return f.running ? "Switch to" : "Open"
    }

    private var hints: [(String, String)] {
        var h: [(String, String)] = [("B", "Back")]
        if let f = focused, f.running, !isDesktop { h.append(("X", "Quit")) }
        h.append(("L1 R1", "Tabs"))
        return h
    }

    private var strip: some View {
        ScrollViewReader { proxy in
            ScrollView(.horizontal, showsIndicators: false) {
                LazyHStack(spacing: 18) {
                    ForEach(Array(current.apps.enumerated()), id: \.element.id) { ci, app in
                        tile(app, focused: ci == col).id(app.id)
                            .onTapGesture { col = ci; activate() }
                    }
                }
                .padding(.horizontal, 40).padding(.vertical, 18)
            }
            .onChange(of: col) { _, _ in
                if let f = focused { withAnimation(.spring(response: 0.3)) { proxy.scrollTo(f.id, anchor: .center) } }
            }
            .onChange(of: tab) { _, _ in
                if let f = focused { proxy.scrollTo(f.id, anchor: .leading) }
            }
        }
        .frame(height: 150)
    }

    private func tile(_ app: AppEntry, focused: Bool) -> some View {
        VStack(spacing: 8) {
            ZStack(alignment: .topTrailing) {
                if app.id == Self.desktop.id {
                    RoundedRectangle(cornerRadius: 22, style: .continuous).fill(Color.white.opacity(0.12))
                        .frame(width: 92, height: 92)
                        .overlay(Image(systemName: "macwindow").font(.system(size: 38, weight: .light)))
                } else if let img = engine.icons[app.id] {
                    Image(uiImage: img).resizable().interpolation(.high).frame(width: 92, height: 92)
                        .clipShape(RoundedRectangle(cornerRadius: 22, style: .continuous))
                } else {
                    RoundedRectangle(cornerRadius: 22, style: .continuous).fill(colour(for: app.name))
                        .frame(width: 92, height: 92)
                        .overlay(Text(String(app.name.prefix(1)).uppercased())
                            .font(.system(size: 40, weight: .bold, design: .rounded)).foregroundStyle(.white))
                }
                if app.running, app.id != Self.desktop.id {
                    Circle().fill(Theme.good).frame(width: 14, height: 14)
                        .overlay(Circle().stroke(Color.black.opacity(0.6), lineWidth: 2)).offset(x: 4, y: -4)
                }
            }
            .overlay(RoundedRectangle(cornerRadius: 22, style: .continuous)
                .stroke(Color.white, lineWidth: focused ? 4 : 0).frame(width: 100, height: 100))
            .shadow(color: focused ? Theme.accent.opacity(0.7) : .clear, radius: 16)
            Text(app.name).font(.system(size: 13, weight: .semibold)).lineLimit(1)
                .foregroundStyle(focused ? Theme.text : Theme.muted).frame(width: 104)
        }
        .scaleEffect(focused ? 1.12 : 1.0)
        .animation(.spring(response: 0.25, dampingFraction: 0.8), value: focused)
    }

    /// A steady colour per name, for apps whose icon has not arrived.
    private func colour(for name: String) -> Color {
        var h: UInt32 = 2166136261
        for b in name.utf8 { h = (h ^ UInt32(b)) &* 16777619 }
        return Color(hue: Double(h % 360) / 360, saturation: 0.55, brightness: 0.75)
    }

    // MARK: Actions

    private func activate() {
        guard let f = focused else { return }
        if isDesktop {
            engine.showDesktop()
        } else {
            engine.open(f, show: engine.status.full_access)
        }
        overlay = .none
    }

    private func handle(_ ev: NavEvent) {
        switch ev {
        case .left: col = max(0, col - 1)
        case .right: col = min(current.apps.count - 1, col + 1)
        case .l1, .up: tab = max(0, tab - 1); col = 0
        case .r1, .down: tab = min(tabs.count - 1, tab + 1); col = 0
        case .confirm: activate()
        case .x:
            if let f = focused, f.running, !isDesktop { engine.quit(f); refreshSoon() }
        case .back: overlay = .none
        case .home: overlay = .quickMenu
        default: break
        }
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
            if engine.apps.isEmpty, engine.status.has_launcher { engine.requestApps() }
        }
    }
}
