// A cheat-sheet of the current layout, laid over the picture: every
// control on the pad and what it does right now. Any button closes it.

import SwiftUI

struct ControlsSheet: View {
    @Binding var overlay: Overlay
    @EnvironmentObject var engine: Engine
    @EnvironmentObject var pad: Pad
    @State private var token = 0

    var body: some View {
        ZStack {
            Color.black.opacity(0.7).ignoresSafeArea().onTapGesture { overlay = .none }
            VStack(alignment: .leading, spacing: 14) {
                HStack {
                    Text(engine.prefs.profile.label).font(.system(size: 24, weight: .bold, design: .rounded))
                    Spacer()
                    Text("Home: quick menu · any button closes this").font(.system(size: 13)).foregroundStyle(Theme.muted)
                }
                if engine.prefs.profile == .controller {
                    Text("The pad is sent as a controller. A Mac cannot show a virtual controller without Apple's entitlement; a PC with ViGEmBus can.")
                        .font(.system(size: 15)).foregroundStyle(Theme.muted)
                } else {
                    let m = engine.prefs.activeMap
                    let all = PadControl.allCases
                    let half = (all.count + 1) / 2
                    HStack(alignment: .top, spacing: 28) {
                        column(Array(all[..<half]), m)
                        column(Array(all[half...]), m)
                    }
                }
            }
            .padding(28)
            .frame(maxWidth: 760)
            .background(Color(red: 0.08, green: 0.09, blue: 0.13).opacity(0.96))
            .clipShape(RoundedRectangle(cornerRadius: 24, style: .continuous))
        }
        .onAppear { token = pad.push { _ in overlay = .none } }
        .onDisappear { pad.pop(token) }
    }

    private func column(_ controls: [PadControl], _ m: ControlMap) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            ForEach(controls, id: \.self) { c in
                HStack {
                    Text(c.label).font(.system(size: 14, weight: .semibold)).frame(width: 170, alignment: .leading)
                    Text(m[c].label).font(.system(size: 14)).foregroundStyle(Theme.accent)
                }
            }
        }
    }
}
