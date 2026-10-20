// Anywhere for iPhone: your Mac, on the phone, driven by the controller
// clipped to it. Landscape only (that is how a Kishi holds the phone).
//
// Screens: Home (your computers) → Pair (the six-digit code) → Stream
// (the Mac's picture), with the quick menu, launcher and keyboard laid
// over the stream. `Router` says which is showing.

import SwiftUI

@main
struct AnywhereApp: App {
    @StateObject private var engine: Engine
    @StateObject private var pad: Pad
    @StateObject private var router = Router()

    init() {
        let e = Engine()
        let p = Pad()
        e.pad = p
        p.mapper.engine = e
        _engine = StateObject(wrappedValue: e)
        _pad = StateObject(wrappedValue: p)
    }

    var body: some Scene {
        WindowGroup {
            RootView()
                .environmentObject(engine)
                .environmentObject(pad)
                .environmentObject(router)
                .preferredColorScheme(.dark)
                .statusBarHidden(true)
                .persistentSystemOverlays(.hidden)
                .onAppear { UIApplication.shared.isIdleTimerDisabled = true }
        }
    }
}

enum Screen: Equatable {
    case home
    case pair
    case stream
    case settings
    case controls
}

@MainActor
final class Router: ObservableObject {
    @Published var screen: Screen = .home
}

struct RootView: View {
    @EnvironmentObject var router: Router

    var body: some View {
        ZStack {
            Theme.bg.ignoresSafeArea()
            switch router.screen {
            case .home: HomeView()
            case .pair: PairView()
            case .stream: StreamScreen()
            case .settings: SettingsView()
            case .controls: ControlsView()
            }
        }
    }
}
