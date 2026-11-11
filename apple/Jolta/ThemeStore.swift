import AppKit
import Observation
import Synchronization

/// The palettes on screen: the chosen dark and light themes, with a theme
/// being previewed in place of the one for its mode.
nonisolated struct ThemePair: Equatable, Sendable {
    var dark: ThemeTokens
    var light: ThemeTokens

    subscript(mode: ThemeMode) -> ThemeTokens {
        get { mode == .dark ? dark : light }
        set { if mode == .dark { dark = newValue } else { light = newValue } }
    }
}

/// The theme choice and the theme being previewed from ⌘K. A choice that
/// shows one theme always, and a preview, force the appearance of that
/// theme's mode on the whole app, so system controls, scroll bars and the
/// reader's web page match it.
@Observable
final class ThemeStore {
    static let shared = ThemeStore()

    private(set) var choice: ThemeChoice
    private(set) var preview: ThemePalette?
    /// What every theme color resolves to right now.
    private(set) var active: ThemePair

    /// AppKit asks dynamic colors for their value whenever it draws, on any
    /// thread, so the colors read the pair here instead of capturing one.
    private nonisolated static let live = Mutex(ThemePair(dark: ThemePalette.graphiteDark.tokens, light: ThemePalette.graphiteLight.tokens))
    @ObservationIgnored private var colors: [[AnyKeyPath]: NSColor] = [:]
    @ObservationIgnored private let keyPrefix: String

    private init() {
        let defaults = UserDefaults.standard
        let test = AppModel.isTestData ? defaults.string(forKey: "JoltaTestData") ?? "" : ""
        let prefix = test.isEmpty ? "" : test + "."
        // UI tests save under their own keys and start from Graphite each launch.
        if !test.isEmpty { ThemeChoice.forget(defaults, prefix: prefix) }
        keyPrefix = prefix
        let choice = ThemeChoice.load(defaults, prefix: prefix)
        self.choice = choice
        active = ThemePair(dark: choice.dark.tokens, light: choice.light.tokens)
        Self.live.withLock { $0 = active }
        force(choice.always)
    }

    /// The tokens on screen for one appearance, for colors mixed from them
    /// while AppKit draws (calendar colors), on whatever thread it draws.
    nonisolated static func tokens(_ mode: ThemeMode) -> ThemeTokens {
        live.withLock { $0[mode] }
    }

    /// macOS's appearance. While Jolta forces its own, the one from just
    /// before, which only matters while the choice matches macOS and a
    /// preview forces another.
    var systemMode: ThemeMode {
        NSApp.appearance == nil ? Self.mode(NSApp.effectiveAppearance) : systemModeBeforeForcing
    }
    @ObservationIgnored private var systemModeBeforeForcing = ThemeMode.dark

    /// The theme on screen, leaving a preview aside.
    var shown: ThemePalette { choice.shown(systemMode) }

    func startPreview(_ palette: ThemePalette) {
        guard preview != palette else { return }
        preview = palette
        apply()
    }

    func endPreview() {
        guard preview != nil else { return }
        preview = nil
        apply()
    }

    /// Changes the choice, saves it and ends any preview.
    func update(_ change: (inout ThemeChoice) -> Void) {
        change(&choice)
        choice.save(UserDefaults.standard, prefix: keyPrefix)
        preview = nil
        apply()
    }

    /// Chooses a theme from ⌘K (`ThemeChoice.pick`). Returns whether the
    /// pick made Jolta stop matching macOS.
    @discardableResult func pick(_ theme: ThemePalette) -> Bool {
        let matched = choice.always == nil
        let system = systemMode
        update { $0.pick(theme, system: system) }
        return matched && choice.always != nil
    }

    private func apply() {
        force(preview?.mode ?? choice.always)
        var pair = ThemePair(dark: choice.dark.tokens, light: choice.light.tokens)
        if let preview { pair[preview.mode] = preview.tokens }
        guard pair != active else { return }
        Self.live.withLock { $0 = pair }
        active = pair
        // New objects, so SwiftUI sees a different Color and draws again.
        colors = [:]
        for window in NSApp.windows {
            if let view = window.contentView { Self.redraw(view) }
        }
    }

    /// Shows the whole app in `mode`, or in macOS's appearance for nil.
    private func force(_ mode: ThemeMode?) {
        let name: NSAppearance.Name? = mode.map { $0 == .dark ? .darkAqua : .aqua }
        guard NSApp.appearance?.name != name else { return }
        if NSApp.appearance == nil { systemModeBeforeForcing = systemMode }
        NSApp.appearance = name.flatMap(NSAppearance.init(named:))
    }

    private static func mode(_ appearance: NSAppearance) -> ThemeMode {
        appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? .dark : .light
    }

    private static func redraw(_ view: NSView) {
        view.needsDisplay = true
        for sub in view.subviews { redraw(sub) }
    }

    /// The color for one token. Reading it from a view body makes the view
    /// draw again when the theme changes. AppKit keeps some of these colors
    /// (in attributed strings, as tints), so each one resolves from the live
    /// pair each time it draws rather than from the pair when it was made.
    func color(_ token: KeyPath<ThemeTokens, UInt32> & Sendable, alpha: (KeyPath<ThemeTokens, Double> & Sendable)? = nil) -> NSColor {
        _ = active
        let key: [AnyKeyPath] = [token] + (alpha.map { [$0] } ?? [])
        if let color = colors[key] { return color }
        let color = Self.dynamicColor(token, alpha: alpha)
        colors[key] = color
        return color
    }

    private nonisolated static func dynamicColor(_ token: KeyPath<ThemeTokens, UInt32> & Sendable, alpha: (KeyPath<ThemeTokens, Double> & Sendable)?) -> NSColor {
        NSColor(name: nil) { appearance in
            let mode: ThemeMode = appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? .dark : .light
            return live.withLock {
                let tokens = $0[mode]
                return NSColor(hex: tokens[keyPath: token], alpha: alpha.map { tokens[keyPath: $0] } ?? 1)
            }
        }
    }
}
