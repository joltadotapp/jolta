import AppKit
import SwiftUI

// MARK: - Scale

extension Theme {
    nonisolated static let caption: CGFloat = 11
    nonisolated static let small: CGFloat = 12
    nonisolated static let body: CGFloat = 13
    nonisolated static let title: CGFloat = 15
    nonisolated static let display: CGFloat = 21

    /// The reader's body text and its line, before `ReadingSize` scales them.
    nonisolated static let readerBody: CGFloat = 14
    nonisolated static let readerLine: CGFloat = 22

    /// A size of the message list's rows or the reader's text at the text
    /// size chosen in Settings › Appearance, to the whole point.
    static func scaled(_ size: CGFloat) -> CGFloat { (size * ThemeStore.shared.textSize.scale).rounded() }

    nonisolated static let radiusKey: CGFloat = 4
    nonisolated static let radiusControl: CGFloat = 6
    nonisolated static let radiusRow: CGFloat = 8
    nonisolated static let radiusPanel: CGFloat = 12

    nonisolated static let controlSmall: CGFloat = 24
    nonisolated static let control: CGFloat = 28

    /// The left edge everything in a column starts at. In the list: its
    /// title, the section tabs, each row's sender and the footer. In the
    /// reader and compose: the toolbar's first icon, the subject, cards,
    /// each message's header and body, the fields and the format bar.
    nonisolated static let gutter: CGFloat = 28

    /// Popovers, panels and overlays appearing.
    static var quick: Animation? { motion(.snappy(duration: 0.16)) }
    /// Toasts and sliding panes.
    static var settle: Animation? { motion(.snappy(duration: 0.22)) }

    /// Nothing moves while macOS's Reduce Motion is on. `quick` and `settle`
    /// are the app's two; the empty inbox's slow fade is the one other caller.
    static func motion(_ animation: Animation) -> Animation? {
        NSWorkspace.shared.accessibilityDisplayShouldReduceMotion ? nil : animation
    }
}

/// Settings › Appearance › Text size: how large the message list's rows
/// and the reader's text are.
nonisolated enum ReadingSize: String, CaseIterable, Identifiable, Sendable {
    case small, standard, large

    var id: String { rawValue }

    var name: String {
        switch self {
        case .small: "Small"
        case .standard: "Default"
        case .large: "Large"
        }
    }

    /// 12, 13 or 15 points for the list's 13, and the rest in step.
    var scale: CGFloat {
        switch self {
        case .small: 12 / Theme.body
        case .standard: 1
        case .large: 15 / Theme.body
        }
    }
}

extension EnvironmentValues {
    /// What the header of the window's first column leaves free on its left
    /// while the sidebar is folded away: the window buttons and the mode
    /// switcher sit there. Zero beside the sidebar.
    @Entry var headerInset: CGFloat = 0
}

extension AnyTransition {
    /// How a panel comes and goes, under `Theme.quick`. `anchor` is the
    /// panel's middle in its own frame, for one placed with an offset.
    static func panel(anchor: UnitPoint = .center) -> AnyTransition {
        .opacity.combined(with: .scale(scale: 0.98, anchor: anchor))
    }
}

// MARK: - Shadows and panels

/// A shadow that is lighter in a light theme, where black spreads further.
private struct KitShadow: ViewModifier {
    let dark: Double
    let light: Double
    let radius: CGFloat
    let y: CGFloat
    @Environment(\.colorScheme) private var scheme

    func body(content: Content) -> some View {
        content.shadow(color: .black.opacity(scheme == .dark ? dark : light), radius: radius, y: y)
    }
}

extension View {
    /// Under a menu, a toast or a small popover.
    func menuShadow() -> some View {
        modifier(KitShadow(dark: 0.28, light: 0.12, radius: 16, y: 6))
    }

    /// Under a panel that floats over the window.
    func panelShadow() -> some View {
        modifier(KitShadow(dark: 0.45, light: 0.18, radius: 32, y: 16))
    }

    private func panelShell() -> some View {
        background(RoundedRectangle(cornerRadius: Theme.radiusPanel).fill(Color(Theme.raise)))
            .overlay(RoundedRectangle(cornerRadius: Theme.radiusPanel).strokeBorder(Color(Theme.line), lineWidth: 1).allowsHitTesting(false))
    }

    /// The shell of a panel that floats over the window: the palette, a picker, an event's detail.
    func panel() -> some View {
        panelShell().panelShadow()
    }

    /// The same shell lying in the page, without a shadow.
    func card() -> some View {
        panelShell()
    }

    /// The shell of a small one: a menu, a toast, a list of suggestions.
    func menuPanel() -> some View {
        panelShell().menuShadow()
    }
}

// MARK: - Key hint

/// A key hint such as `E` or `⌘K`.
struct KeyCap: View {
    enum Style {
        /// A filled plate, on any surface.
        case plate
        /// Outlined in the accent: the key of the thing the keyboard is on.
        case focused
        /// Outlined in the text color around it, for a filled button.
        case inherit
    }

    let key: String
    var style = Style.plate

    init(key: String, style: Style = .plate) {
        self.key = key
        self.style = style
    }

    init(key: String, focused: Bool) {
        self.init(key: key, style: focused ? .focused : .plate)
    }

    var body: some View {
        Text(key)
            .font(.system(size: 10.5, weight: .medium))
            .foregroundStyle(ink)
            .padding(.horizontal, 4)
            .frame(minWidth: 17, minHeight: 17)
            .background {
                let box = RoundedRectangle(cornerRadius: Theme.radiusKey)
                switch style {
                case .plate: box.fill(Color(Theme.kbd))
                case .focused: box.strokeBorder(Color(Theme.accent), lineWidth: 1)
                case .inherit: box.strokeBorder(ForegroundStyle.foreground.opacity(0.5), lineWidth: 1)
                }
            }
    }

    private var ink: AnyShapeStyle {
        switch style {
        case .plate: AnyShapeStyle(Color(Theme.kbdText))
        case .focused: AnyShapeStyle(Color(Theme.accentText))
        case .inherit: AnyShapeStyle(ForegroundStyle.foreground.opacity(0.8))
        }
    }

    /// A keymap key as its cap shows it: a letter in capitals, `esc` and `⌘K` as written.
    static func shown(_ key: String) -> String {
        key.count == 1 ? key.uppercased() : key
    }
}

// MARK: - Button

/// The app's button. `key` shows the key that presses it; the call site
/// binds the shortcut.
struct KitButton: View {
    enum Kind {
        /// The one action a panel is for.
        case primary
        case secondary
        /// Takes something away for good.
        case danger
        /// No fill until hovered, for a toolbar.
        case quiet
    }

    enum Size { case regular, small }

    let title: String
    var key: String?
    var icon: String?
    var kind = Kind.secondary
    var size = Size.regular
    /// False leaves the icon to stand for the title.
    var showsTitle = true
    var showsKey = true
    var iconTint: Color?
    let action: () -> Void
    @FocusState private var focused: Bool

    var body: some View {
        Button(action: action) {
            HStack(spacing: size == .regular ? 7 : 5) {
                if let icon {
                    Image(systemName: icon)
                        .font(.system(size: size == .regular ? Theme.body : Theme.small))
                        .foregroundStyle(iconTint.map { AnyShapeStyle($0) } ?? AnyShapeStyle(ForegroundStyle.foreground))
                }
                if showsTitle { Text(title).lineLimit(1) }
                if let key, showsKey { KeyCap(key: key, style: .inherit) }
            }
        }
        .buttonStyle(KitButtonStyle(kind: kind, size: size, trailingKey: key != nil && showsKey, focused: focused))
        .focused($focused)
        .focusEffectDisabled()
        .fixedSize()
        .accessibilityLabel(title)
    }
}

private struct KitButtonStyle: ButtonStyle {
    let kind: KitButton.Kind
    let size: KitButton.Size
    let trailingKey: Bool
    let focused: Bool

    func makeBody(configuration: Configuration) -> some View {
        Face(kind: kind, size: size, trailingKey: trailingKey, focused: focused, pressed: configuration.isPressed, label: configuration.label)
    }

    private struct Face<Label: View>: View {
        let kind: KitButton.Kind
        let size: KitButton.Size
        let trailingKey: Bool
        let focused: Bool
        let pressed: Bool
        let label: Label
        @Environment(\.isEnabled) private var enabled
        @State private var hovering = false

        private var ink: Color {
            switch kind {
            case .primary: Color(Theme.bg)
            case .secondary: Color(Theme.text)
            case .danger: .white
            case .quiet: Color(Theme.text2)
            }
        }

        private var fill: Color {
            switch kind {
            case .primary: Color(Theme.text)
            case .secondary: Color(Theme.kbd)
            case .danger: Color(Theme.danger)
            case .quiet: .clear
            }
        }

        /// Laid over the fill while the pointer is on the button.
        private var hoverTint: Color {
            switch kind {
            case .primary: Color(Theme.bg).opacity(0.14)
            case .danger: .white.opacity(0.12)
            case .secondary, .quiet: Color(Theme.hover)
            }
        }

        private var edge: CGFloat {
            switch (kind, size) {
            case (.quiet, _): 7
            case (_, .regular): 12
            case (_, .small): 10
            }
        }

        var body: some View {
            let box = RoundedRectangle(cornerRadius: Theme.radiusControl)
            label
                .font(.system(size: size == .regular ? Theme.body : Theme.small, weight: kind == .quiet ? .regular : .medium))
                .foregroundStyle(ink)
                .padding(.leading, edge)
                .padding(.trailing, trailingKey && kind != .quiet ? edge - 4 : edge)
                .frame(height: size == .regular ? Theme.control : Theme.controlSmall)
                .background {
                    box.fill(fill)
                    if hovering && enabled { box.fill(hoverTint) }
                    if kind == .secondary { box.strokeBorder(Color(Theme.line), lineWidth: 1) }
                }
                .overlay {
                    if focused {
                        RoundedRectangle(cornerRadius: Theme.radiusControl + 3).strokeBorder(Color(Theme.accent), lineWidth: 2).padding(-3)
                    }
                }
                .opacity(enabled ? (pressed ? 0.8 : 1) : 0.45)
                .contentShape(Rectangle())
                .onHover { hovering = $0 }
        }
    }
}

// MARK: - Field

extension View {
    /// The box around a text field or a value that opens a picker: a
    /// hairline that turns to the accent while the keyboard is in it. The
    /// field's caret is the accent too.
    func fieldBox(focused: Bool = false, width: CGFloat? = nil, height: CGFloat? = Theme.control, chevrons: Bool = false,
                  fill: NSColor = Theme.raise) -> some View {
        padding(.horizontal, 8)
            .padding(.trailing, chevrons ? 14 : 0)
            .frame(width: width, height: height)
            .background(RoundedRectangle(cornerRadius: Theme.radiusControl).fill(Color(fill)))
            .overlay(RoundedRectangle(cornerRadius: Theme.radiusControl).strokeBorder(Color(focused ? Theme.accent : Theme.line), lineWidth: 1))
            .overlay(alignment: .trailing) {
                if chevrons {
                    Image(systemName: "chevron.up.chevron.down").font(.system(size: 8, weight: .semibold)).foregroundStyle(Color(Theme.text3)).padding(.trailing, 7).allowsHitTesting(false)
                }
            }
    }
}

// MARK: - Selected row

extension View {
    /// A list row's background: filled when it is the selection, tinted
    /// under the pointer. `marked` adds the accent caret, for a row the
    /// keyboard is on.
    func selectedRow(_ on: Bool, marked: Bool = true, hovering: Bool = false, radius: CGFloat = Theme.radiusRow) -> some View {
        background(RoundedRectangle(cornerRadius: radius).fill(Color(on ? Theme.fill : hovering ? Theme.hover : .clear)))
            .overlay(alignment: .leading) {
                if on && marked {
                    Image(systemName: "arrowtriangle.right.fill")
                        .font(.system(size: 7))
                        .foregroundStyle(Color(Theme.accent))
                        .padding(.leading, 1.5)
                        .allowsHitTesting(false)
                        .accessibilityHidden(true)
                }
            }
    }
}
