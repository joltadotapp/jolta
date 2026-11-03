import AppKit
import Observation
import Synchronization

/// The palettes on screen: the saved dark and light themes, with a theme
/// being previewed in place of the one for its mode.
nonisolated struct ThemePair: Equatable, Sendable {
    var dark: ThemeTokens
    var light: ThemeTokens

    subscript(mode: ThemeMode) -> ThemeTokens {
        get { mode == .dark ? dark : light }
        set { if mode == .dark { dark = newValue } else { light = newValue } }
    }
}

/// The saved dark and light themes and the one being previewed from ⌘K.
/// macOS's appearance picks between the two; a preview forces the
/// appearance of its own mode so a dark theme shows on a light Mac.
@Observable
final class ThemeStore {
    static let shared = ThemeStore()

    private(set) var darkID: String
    private(set) var lightID: String
    private(set) var preview: ThemePalette?
    /// What every theme color resolves to right now.
    private(set) var active: ThemePair

    /// AppKit asks dynamic colors for their value whenever it draws, on any
    /// thread, so the colors read the pair here instead of capturing one.
    private nonisolated static let live = Mutex(ThemePair(dark: ThemePalette.graphiteDark.tokens, light: ThemePalette.graphiteLight.tokens))
    @ObservationIgnored private var colors: [KeyPath<ThemeTokens, UInt32>: NSColor] = [:]
    @ObservationIgnored private let defaults = UserDefaults.standard
    @ObservationIgnored private let keyPrefix: String

    private init() {
        // UI tests save under their own keys and start from Graphite each launch.
        let test = UserDefaults.standard.string(forKey: "JoltaTestData") ?? ""
        keyPrefix = test.isEmpty ? "" : test + "."
        if !test.isEmpty {
            defaults.removeObject(forKey: keyPrefix + "ThemeDark")
            defaults.removeObject(forKey: keyPrefix + "ThemeLight")
        }
        let dark = defaults.string(forKey: keyPrefix + "ThemeDark").flatMap(ThemePalette.named) ?? .graphiteDark
        let light = defaults.string(forKey: keyPrefix + "ThemeLight").flatMap(ThemePalette.named) ?? .graphiteLight
        darkID = dark.id
        lightID = light.id
        active = ThemePair(dark: dark.tokens, light: light.tokens)
        Self.live.withLock { $0 = active }
    }

    var dark: ThemePalette { ThemePalette.named(darkID) ?? .graphiteDark }
    var light: ThemePalette { ThemePalette.named(lightID) ?? .graphiteLight }

    /// The mode macOS shows Jolta in, leaving a preview's forced appearance aside.
    var systemMode: ThemeMode {
        guard NSApp.appearance == nil else { return systemModeBeforePreview }
        return NSApp.effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? .dark : .light
    }
    @ObservationIgnored private var systemModeBeforePreview = ThemeMode.dark

    /// The saved theme macOS's appearance shows now.
    var shown: ThemePalette { systemMode == .dark ? dark : light }

    func isSaved(_ palette: ThemePalette) -> Bool {
        palette.id == (palette.mode == .dark ? darkID : lightID)
    }

    func startPreview(_ palette: ThemePalette) {
        guard preview != palette else { return }
        if preview == nil { systemModeBeforePreview = systemMode }
        preview = palette
        NSApp.appearance = NSAppearance(named: palette.mode == .dark ? .darkAqua : .aqua)
        apply()
    }

    func endPreview() {
        guard preview != nil else { return }
        preview = nil
        NSApp.appearance = nil
        apply()
    }

    /// Makes `palette` the theme for its mode and ends any preview.
    func save(_ palette: ThemePalette) {
        if palette.mode == .dark { darkID = palette.id } else { lightID = palette.id }
        defaults.set(palette.id, forKey: keyPrefix + (palette.mode == .dark ? "ThemeDark" : "ThemeLight"))
        preview = nil
        NSApp.appearance = nil
        apply()
    }

    private func apply() {
        var pair = ThemePair(dark: dark.tokens, light: light.tokens)
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

    private static func redraw(_ view: NSView) {
        view.needsDisplay = true
        for sub in view.subviews { redraw(sub) }
    }

    /// The color for one token. Reading it from a view body makes the view
    /// draw again when the theme changes. AppKit keeps some of these colors
    /// (in attributed strings, as tints), so each one resolves from the live
    /// pair each time it draws rather than from the pair when it was made.
    func color(_ token: KeyPath<ThemeTokens, UInt32> & Sendable) -> NSColor {
        _ = active
        if let color = colors[token] { return color }
        let color = Self.dynamicColor(token)
        colors[token] = color
        return color
    }

    private nonisolated static func dynamicColor(_ token: KeyPath<ThemeTokens, UInt32> & Sendable) -> NSColor {
        NSColor(name: nil) { appearance in
            let mode: ThemeMode = appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? .dark : .light
            return NSColor(hex: live.withLock { $0[mode][keyPath: token] })
        }
    }
}
