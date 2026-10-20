// The look: a console home screen in macOS clothes. Dark glass, big
// rounded tiles, one bright focus ring that the controller moves. Every
// screen uses these pieces so the whole app feels like one thing.

import SwiftUI

enum Theme {
    static let bg = LinearGradient(colors: [Color(red: 0.07, green: 0.08, blue: 0.12), Color(red: 0.11, green: 0.09, blue: 0.18)],
                                   startPoint: .topLeading, endPoint: .bottomTrailing)
    static let glass = Color.white.opacity(0.08)
    static let glassStrong = Color.white.opacity(0.16)
    static let text = Color.white
    static let muted = Color.white.opacity(0.62)
    static let accent = Color(red: 0.38, green: 0.55, blue: 1.0)
    static let good = Color(red: 0.30, green: 0.85, blue: 0.50)
    static let warn = Color(red: 1.0, green: 0.72, blue: 0.30)
    static let corner: CGFloat = 22
}

/// A tile that glows when the controller's focus is on it.
struct FocusTile<Content: View>: View {
    let focused: Bool
    var width: CGFloat? = nil
    var height: CGFloat? = nil
    @ViewBuilder let content: () -> Content

    var body: some View {
        content()
            .frame(width: width, height: height)
            .background(focused ? Theme.glassStrong : Theme.glass)
            .clipShape(RoundedRectangle(cornerRadius: Theme.corner, style: .continuous))
            .overlay(
                RoundedRectangle(cornerRadius: Theme.corner, style: .continuous)
                    .stroke(focused ? Color.white : Color.white.opacity(0.12), lineWidth: focused ? 3 : 1)
            )
            .shadow(color: focused ? Theme.accent.opacity(0.55) : .clear, radius: 18)
            .scaleEffect(focused ? 1.05 : 1.0)
            .animation(.spring(response: 0.22, dampingFraction: 0.8), value: focused)
    }
}

/// The glyph on a controller button, for the hints along the bottom.
struct ButtonHint: View {
    let glyph: String
    let text: String

    var body: some View {
        HStack(spacing: 6) {
            Text(glyph)
                .font(.system(size: 13, weight: .bold, design: .rounded))
                .frame(width: 24, height: 24)
                .background(Circle().fill(Color.white.opacity(0.18)))
            Text(text).font(.system(size: 14, weight: .medium)).foregroundStyle(Theme.muted)
        }
    }
}

struct HintBar: View {
    let hints: [(String, String)]

    var body: some View {
        HStack(spacing: 18) {
            ForEach(Array(hints.enumerated()), id: \.offset) { _, h in
                ButtonHint(glyph: h.0, text: h.1)
            }
        }
        .padding(.horizontal, 18)
        .padding(.vertical, 10)
        .background(Capsule().fill(Color.black.opacity(0.35)))
    }
}

/// Moves a focus index around a grid laid out in rows of `columns`.
enum FocusMove {
    static func step(_ index: Int, _ dir: NavEvent, columns: Int, count: Int) -> Int {
        guard count > 0 else { return 0 }
        let c = max(1, columns)
        var i = index
        switch dir {
        case .left: if i % c > 0 { i -= 1 }
        case .right: if i % c < c - 1, i + 1 < count { i += 1 }
        case .up: if i - c >= 0 { i -= c }
        case .down: if i + c < count { i += c } else if i / c < (count - 1) / c { i = count - 1 }
        default: break
        }
        return max(0, min(count - 1, i))
    }
}

/// A big friendly message in the middle of the screen.
struct Notice: View {
    let symbol: String
    let title: String
    var detail: String = ""

    var body: some View {
        VStack(spacing: 12) {
            Image(systemName: symbol).font(.system(size: 44)).foregroundStyle(Theme.muted)
            Text(title).font(.system(size: 26, weight: .semibold, design: .rounded)).foregroundStyle(Theme.text)
            if !detail.isEmpty {
                Text(detail).font(.system(size: 15)).foregroundStyle(Theme.muted).multilineTextAlignment(.center)
                    .frame(maxWidth: 520)
            }
        }
        .padding(32)
    }
}
