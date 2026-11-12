import SwiftUI

/// Settings › Appearance: a theme for each macOS appearance, or one theme
/// always. The page answers the arrow keys on its own, so it works from the
/// keyboard without macOS's keyboard navigation: ↑↓ move between the rows,
/// ←→ change the one marked.
struct AppearanceSettings: View {
    private let store = ThemeStore.shared
    @Environment(\.colorScheme) private var scheme
    @FocusState private var focused: Bool
    @State private var marked = Row.follow

    private enum Row: Hashable { case follow, light, dark, always }

    private var choice: ThemeChoice { store.choice }

    private var rows: [Row] { choice.fixed == nil ? [.follow, .light, .dark] : [.follow, .always] }

    private var current: Row { rows.contains(marked) ? marked : .follow }

    private var footer: String {
        if let fixed = choice.fixed {
            return "Jolta shows \(fixed.name) whether macOS is light or dark. Its windows, menus and scroll bars stay \(fixed.mode.rawValue) too."
        }
        // While Jolta matches macOS, the window's scheme is macOS's.
        let shown = choice.shown(scheme == .dark ? .dark : .light)
        return "Jolta switches between these with macOS. It shows \(shown.name) now."
    }

    var body: some View {
        SettingsScroll {
            VStack(alignment: .leading, spacing: 14) {
                SettingsSection("Theme", footer: footer) {
                    SettingsBlock {
                        HStack(spacing: 8) {
                            ChoiceCard(title: "Match macOS", detail: "One theme for light mode, one for dark",
                                       icon: "circle.lefthalf.filled", selected: choice.fixed == nil) { follow(true) }
                            ChoiceCard(title: "Always use one theme", detail: "The same theme in light and dark mode",
                                       icon: "paintpalette", selected: choice.fixed != nil) { follow(false) }
                        }
                    }
                    .modifier(Marked(on: focused && current == .follow))
                    if let fixed = choice.fixed {
                        themeRow(.always, title: "Theme", detail: "Every theme, light and dark.", selected: fixed)
                    } else {
                        themeRow(.light, title: "Light", detail: "While macOS is in light mode.", selected: choice.light)
                        themeRow(.dark, title: "Dark", detail: "While macOS is in dark mode.", selected: choice.dark)
                    }
                }
                HStack(spacing: 16) {
                    HStack(spacing: 5) { KeyCap(key: "↑"); KeyCap(key: "↓"); Text("choose a row") }
                    HStack(spacing: 5) { KeyCap(key: "←"); KeyCap(key: "→"); Text("change it") }
                }
                .font(.system(size: Theme.small))
                .foregroundStyle(Color(Theme.text3))
                .padding(.horizontal, 4)
                .opacity(focused ? 1 : 0)
            }
            .focusable()
            .focused($focused)
            .focusEffectDisabled()
            .onKeyPress(keys: [.upArrow, .downArrow, .leftArrow, .rightArrow]) { press in
                switch press.key {
                case .upArrow: move(-1)
                case .downArrow: move(1)
                case .leftArrow: change(-1)
                default: change(1)
                }
                return .handled
            }
            .onAppear { focused = true }
        }
    }

    private func themes(_ row: Row) -> [ThemePalette] {
        switch row {
        case .light: ThemePalette.all.filter { $0.mode == .light }
        case .dark: ThemePalette.all.filter { $0.mode == .dark }
        case .always, .follow: ThemePalette.all
        }
    }

    private func themeRow(_ row: Row, title: String, detail: String, selected: ThemePalette) -> some View {
        SettingsRow(title: title, detail: detail) {
            Picker(title, selection: Binding(get: { selected.id }, set: { id in ThemePalette.named(id).map { choose($0, in: row) } })) {
                ForEach(themes(row)) { theme in
                    Label { Text(theme.name) } icon: { Image(nsImage: Self.swatchImage(theme)).renderingMode(.original) }
                        .tag(theme.id)
                }
            }
            .settingsMenu()
        }
        .modifier(Marked(on: focused && current == row))
    }

    private func choose(_ theme: ThemePalette, in row: Row) {
        store.update { row == .always ? $0.showAlways(theme) : $0.set(theme) }
    }

    /// "Always use one theme" starts from the theme on screen, so nothing changes until another is chosen.
    private func follow(_ macOS: Bool) {
        let shown = store.shown
        store.update { macOS ? $0.matchMacOS() : $0.showAlways(shown) }
    }

    private func move(_ step: Int) {
        let i = rows.firstIndex(of: current) ?? 0
        marked = rows[min(max(i + step, 0), rows.count - 1)]
    }

    private func change(_ step: Int) {
        if current == .follow { return follow(step < 0) }
        let selected = current == .always ? choice.fixed : current == .dark ? choice.dark : choice.light
        let list = themes(current)
        guard let i = list.firstIndex(where: { $0 == selected }) else { return }
        choose(list[min(max(i + step, 0), list.count - 1)], in: current)
    }

    /// A menu item shows an image rather than a view, so the swatch is drawn
    /// as one, with room after it because the menu sets the name right beside it.
    private static func swatchImage(_ theme: ThemePalette) -> NSImage {
        let image = NSImage(size: NSSize(width: 23, height: 8), flipped: false) { _ in
            for (i, color) in [theme.tokens.bg, theme.tokens.text, theme.tokens.accent].enumerated() {
                let dot = NSBezierPath(ovalIn: NSRect(x: CGFloat(i) * 5 + 0.5, y: 0.5, width: 7, height: 7))
                NSColor(hex: color).setFill()
                dot.fill()
                NSColor.separatorColor.setStroke()
                dot.stroke()
            }
            return true
        }
        image.isTemplate = false
        return image
    }
}

/// The row the arrow keys change: filled, with the accent marker the palette uses.
private struct Marked: ViewModifier {
    let on: Bool

    func body(content: Content) -> some View {
        // Inset, so the fill stays clear of the card's edge and its dividers.
        content.background { Color.clear.selectedRow(on, radius: Theme.radiusControl).padding(3) }
    }
}
