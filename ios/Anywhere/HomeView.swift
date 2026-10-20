// Home: your computers as big cards, like a console's game row. The
// controller moves the focus; A connects. Every few seconds the app asks
// "who's there?" so each card shows whether that Mac is reachable now
// (on this Wi-Fi, or through Tailscale from anywhere).

import SwiftUI

struct HomeView: View {
    @EnvironmentObject var engine: Engine
    @EnvironmentObject var pad: Pad
    @EnvironmentObject var router: Router

    @State private var hosts: [PairedHost] = []
    @State private var online: [String: FoundHost] = [:]
    @State private var focus = 0
    @State private var token = 0
    @State private var confirmForget: String?
    @State private var searching = true

    /// Cards: every paired computer, then "Add", then "Settings".
    private var count: Int { hosts.count + 2 }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .firstTextBaseline) {
                Text("Anywhere").font(.system(size: 34, weight: .bold, design: .rounded)).foregroundStyle(Theme.text)
                Spacer()
                padBadge
            }
            .padding(.horizontal, 36)
            .padding(.top, 22)

            Text(hosts.isEmpty ? "Add a computer to get started" : (searching ? "Looking for your computers…" : "Your computers"))
                .font(.system(size: 15, weight: .medium)).foregroundStyle(Theme.muted)
                .padding(.horizontal, 36).padding(.top, 4)

            ScrollViewReader { proxy in
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(spacing: 22) {
                        ForEach(Array(hosts.enumerated()), id: \.element.id) { i, h in
                            hostCard(h, focused: focus == i).id(i)
                                .onTapGesture { focus = i; connect(h) }
                        }
                        FocusTile(focused: focus == hosts.count, width: 220, height: 200) {
                            VStack(spacing: 12) {
                                Image(systemName: "plus.circle.fill").font(.system(size: 44)).foregroundStyle(Theme.accent)
                                Text("Add a computer").font(.system(size: 18, weight: .semibold, design: .rounded))
                                Text("Pair with a code").font(.system(size: 13)).foregroundStyle(Theme.muted)
                            }
                        }
                        .id(hosts.count)
                        .onTapGesture { router.screen = .pair }
                        FocusTile(focused: focus == hosts.count + 1, width: 220, height: 200) {
                            VStack(spacing: 12) {
                                Image(systemName: "gearshape.fill").font(.system(size: 44)).foregroundStyle(Theme.muted)
                                Text("Settings").font(.system(size: 18, weight: .semibold, design: .rounded))
                                Text("Picture, sound, controls").font(.system(size: 13)).foregroundStyle(Theme.muted)
                            }
                        }
                        .id(hosts.count + 1)
                        .onTapGesture { router.screen = .settings }
                    }
                    .padding(.horizontal, 36)
                    .padding(.vertical, 28)
                }
                .onChange(of: focus) { _, f in withAnimation { proxy.scrollTo(f, anchor: .center) } }
            }

            Spacer()
            HStack {
                HintBar(hints: hints)
                Spacer()
                Text("v\(engine.version)").font(.system(size: 12)).foregroundStyle(Theme.muted.opacity(0.6))
            }
            .padding(.horizontal, 36).padding(.bottom, 18)
        }
        .onAppear {
            pad.mode = .ui
            hosts = engine.paired()
            token = pad.push(handle)
            Task { @MainActor in await refreshLoop() }
        }
        .onDisappear { pad.pop(token) }
    }

    private var hints: [(String, String)] {
        if focus < hosts.count {
            return [("A", "Connect"), ("Y", confirmForget == nil ? "Forget" : "Press again to forget"), ("X", "Settings")]
        }
        return [("A", "Select"), ("X", "Settings")]
    }

    private var padBadge: some View {
        HStack(spacing: 8) {
            Image(systemName: pad.connected ? "gamecontroller.fill" : "gamecontroller")
            Text(pad.connected ? pad.name : "No controller — touch works too").font(.system(size: 13, weight: .medium))
        }
        .foregroundStyle(pad.connected ? Theme.good : Theme.muted)
        .padding(.horizontal, 12).padding(.vertical, 6)
        .background(Capsule().fill(Theme.glass))
    }

    private func hostCard(_ h: PairedHost, focused: Bool) -> some View {
        let found = online[h.key]
        return FocusTile(focused: focused, width: 300, height: 200) {
            VStack(alignment: .leading, spacing: 10) {
                HStack {
                    Image(systemName: "desktopcomputer").font(.system(size: 36)).foregroundStyle(Theme.accent)
                    Spacer()
                    Circle().fill(found == nil ? Color.gray.opacity(0.5) : Theme.good).frame(width: 12, height: 12)
                }
                Spacer()
                Text(h.name).font(.system(size: 22, weight: .bold, design: .rounded)).lineLimit(1)
                Text(statusLine(h, found)).font(.system(size: 13)).foregroundStyle(Theme.muted).lineLimit(2)
            }
            .padding(20)
        }
    }

    private func statusLine(_ h: PairedHost, _ f: FoundHost?) -> String {
        if let f {
            return f.tailscale ? "Reachable through Tailscale" : "On this Wi-Fi · \(f.addr)"
        }
        if searching { return "Looking…" }
        return "Not answering. Is it sharing, and are you on the same Wi-Fi or Tailscale? If iOS asked about the local network and you said no: Settings → Anywhere → Local Network."
    }

    private func handle(_ ev: NavEvent) {
        switch ev {
        case .left: focus = max(0, focus - 1)
        case .right: focus = min(count - 1, focus + 1)
        case .confirm:
            if focus < hosts.count { connect(hosts[focus]) }
            else if focus == hosts.count { router.screen = .pair }
            else { router.screen = .settings }
        case .x: router.screen = .settings
        case .y:
            guard focus < hosts.count else { return }
            let h = hosts[focus]
            if confirmForget == h.key {
                engine.forget(h)
                hosts = engine.paired()
                confirmForget = nil
                focus = min(focus, count - 1)
            } else {
                confirmForget = h.key
            }
        case .home: break
        default: confirmForget = nil
        }
    }

    private func connect(_ h: PairedHost) {
        engine.connect(host: "key:\(h.key)")
        router.screen = .stream
    }

    private func refreshLoop() async {
        while router.screen == .home {
            let found = await engine.discover()
            var map: [String: FoundHost] = [:]
            for f in found where !f.key.isEmpty { map[f.key] = f }
            online = map
            searching = false
            hosts = engine.paired()
            try? await Task.sleep(nanoseconds: 3_000_000_000)
        }
    }
}
