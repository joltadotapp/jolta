import Foundation

/// Which theme Jolta shows: a theme for each macOS appearance, and
/// optionally one of the two shown always, whatever macOS shows. The theme
/// shown always keeps its place in the pair, so matching macOS again brings
/// the other one back.
nonisolated struct ThemeChoice: Equatable, Sendable {
    private(set) var light = ThemePalette.graphiteLight
    private(set) var dark = ThemePalette.graphiteDark
    /// Set while one theme shows in both appearances: that theme's mode.
    private(set) var always: ThemeMode?

    /// How a theme is in use, for labels.
    enum Use: Equatable {
        case light, dark, always

        var label: String {
            switch self {
            case .light: "Light mode"
            case .dark: "Dark mode"
            case .always: "Always"
            }
        }
    }

    private(set) subscript(mode: ThemeMode) -> ThemePalette {
        get { mode == .dark ? dark : light }
        set { if mode == .dark { dark = newValue } else { light = newValue } }
    }

    /// The theme on screen while macOS is in `system` mode.
    func shown(_ system: ThemeMode) -> ThemePalette { self[always ?? system] }

    /// The theme shown in both appearances, if there is one.
    var fixed: ThemePalette? { always.map { self[$0] } }

    func use(of theme: ThemePalette) -> Use? {
        if let fixed { return fixed == theme ? .always : nil }
        guard self[theme.mode] == theme else { return nil }
        return theme.mode == .dark ? .dark : .light
    }

    /// Makes `theme` the theme for its own appearance.
    mutating func set(_ theme: ThemePalette) {
        self[theme.mode] = theme
    }

    /// Shows `theme` in both appearances.
    mutating func showAlways(_ theme: ThemePalette) {
        self[theme.mode] = theme
        always = theme.mode
    }

    mutating func matchMacOS() {
        always = nil
    }

    /// A theme chosen from ⌘K shows at once. While Jolta matches macOS and
    /// the theme is of macOS's mode, it takes that appearance's place;
    /// otherwise Jolta shows it always.
    mutating func pick(_ theme: ThemePalette, system: ThemeMode) {
        if always == nil && theme.mode == system { set(theme) } else { showAlways(theme) }
    }

    // MARK: Saving

    private static func key(_ prefix: String) -> String { prefix + "Theme" }
    /// Where builds before the choice saved the theme for each appearance.
    private static func legacyKey(_ mode: ThemeMode, _ prefix: String) -> String {
        prefix + (mode == .dark ? "ThemeDark" : "ThemeLight")
    }

    /// `["light": id, "dark": id, "always": "dark"]`, as saved in UserDefaults.
    var saved: [String: String] {
        var out = ["light": light.id, "dark": dark.id]
        out["always"] = always?.rawValue
        return out
    }

    /// Reads `saved`. A theme that no longer exists, or one of the other
    /// mode, gives Graphite for that appearance.
    init(saved: [String: String]) {
        for mode in [ThemeMode.light, .dark] {
            if let theme = saved[mode.rawValue].flatMap(ThemePalette.named), theme.mode == mode { self[mode] = theme }
        }
        always = saved["always"].flatMap(ThemeMode.init(rawValue:))
    }

    init() {}

    /// The choice saved under `prefix`, or the one made from the two themes
    /// earlier builds saved, which matches macOS.
    static func load(_ defaults: UserDefaults, prefix: String) -> ThemeChoice {
        if let saved = defaults.dictionary(forKey: key(prefix)) as? [String: String] { return ThemeChoice(saved: saved) }
        var legacy: [String: String] = [:]
        for mode in [ThemeMode.light, .dark] { legacy[mode.rawValue] = defaults.string(forKey: legacyKey(mode, prefix)) }
        return ThemeChoice(saved: legacy)
    }

    func save(_ defaults: UserDefaults, prefix: String) {
        defaults.set(saved, forKey: Self.key(prefix))
        for mode in [ThemeMode.light, .dark] { defaults.removeObject(forKey: Self.legacyKey(mode, prefix)) }
    }

    static func forget(_ defaults: UserDefaults, prefix: String) {
        defaults.removeObject(forKey: key(prefix))
        for mode in [ThemeMode.light, .dark] { defaults.removeObject(forKey: legacyKey(mode, prefix)) }
    }
}
