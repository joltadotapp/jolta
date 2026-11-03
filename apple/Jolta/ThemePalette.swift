import Foundation

nonisolated enum ThemeMode: String, Sendable {
    case dark, light
}

/// The colors every view draws with, as sRGB `0xRRGGBB`.
nonisolated struct ThemeTokens: Equatable, Sendable {
    var bg, side, reader, raise, line, line2, text, text2, text3, fill, hover, accent: UInt32
    /// A starred conversation's star: the app icon's amber in Graphite, not a second accent.
    var star: UInt32
    var accentText, kbd, kbdText: UInt32
}

nonisolated struct ThemePalette: Identifiable, Equatable, Sendable {
    let id: String
    let name: String
    let mode: ThemeMode
    let tokens: ThemeTokens

    /// Graphite (docs/design/direction-a.html) first, then Omarchy's themes by name.
    static let all: [ThemePalette] = [graphiteDark, graphiteLight] + OmarchyTheme.all.map {
        ThemePalette(id: $0.id, name: $0.name, mode: $0.mode, tokens: ThemeTokens(omarchy: $0))
    }

    static func named(_ id: String) -> ThemePalette? { all.first { $0.id == id } }

    static let graphiteDark = ThemePalette(id: "graphite-dark", name: "Graphite Dark", mode: .dark, tokens: ThemeTokens(
        bg: 0x121214, side: 0x0C0C0E, reader: 0x161619, raise: 0x1D1D21, line: 0x26262B, line2: 0x1C1C20,
        text: 0xECECEE, text2: 0xA6A6AE, text3: 0x8D8D97, fill: 0x202025, hover: 0x18181C, accent: 0xFF6B2C,
        star: 0xFFC233, accentText: 0xFF8A55, kbd: 0x2A2A30, kbdText: 0xB4B4BC))

    static let graphiteLight = ThemePalette(id: "graphite-light", name: "Graphite Light", mode: .light, tokens: ThemeTokens(
        bg: 0xFBFBFA, side: 0xF3F3F1, reader: 0xFFFFFF, raise: 0xFFFFFF, line: 0xE5E5E2, line2: 0xEFEFEC,
        text: 0x17171A, text2: 0x55555D, text3: 0x66666E, fill: 0xEEEEEB, hover: 0xF5F5F3, accent: 0xE5521A,
        star: 0xD99A00, accentText: 0xB23A0A, kbd: 0xECECEA, kbdText: 0x4A4A52))
}

/// The colors of an Omarchy theme that Jolta's tokens are made from.
/// `OmarchyThemes.swift` holds them, written by `scripts/omarchy-themes.py`.
nonisolated struct OmarchyTheme: Sendable {
    let id: String
    let name: String
    let mode: ThemeMode
    let background, darkBackground, lighterBackground, foreground, selection, accent, yellow: UInt32
}

nonisolated extension ThemeTokens {
    /// Omarchy's own dimmer text colors fall below 4.5:1 in most themes, so
    /// the secondary text colors are mixed from the foreground and pulled back
    /// toward it until they read.
    init(omarchy o: OmarchyTheme) {
        let bg = o.background, fg = o.foreground
        let dark = o.mode == .dark
        let text2 = Self.readable(mix(fg, bg, 0.3), toward: fg, on: bg, contrast: 6)
        self.init(
            bg: bg,
            side: o.darkBackground,
            reader: dark ? mix(bg, o.lighterBackground, 0.3) : mix(bg, 0xFFFFFF, 0.5),
            raise: dark ? o.lighterBackground : mix(bg, 0xFFFFFF, 0.7),
            line: mix(bg, fg, 0.12),
            line2: mix(bg, fg, 0.07),
            text: fg,
            text2: text2,
            text3: Self.readable(mix(fg, bg, 0.45), toward: fg, on: bg, contrast: 4.5),
            fill: contrast(o.selection, bg) >= 1.15 ? o.selection : mix(bg, fg, 0.09),
            hover: mix(bg, fg, 0.04),
            accent: o.accent,
            star: Self.readable(o.yellow, toward: fg, on: bg, contrast: 3),
            accentText: Self.readable(o.accent, toward: fg, on: bg, contrast: 4.5),
            kbd: mix(bg, fg, 0.13),
            kbdText: text2)
    }

    /// `color` moved toward `target` in 1% steps until it has `contrast`
    /// against `bg`; `target` itself when nothing on the way does.
    private static func readable(_ color: UInt32, toward target: UInt32, on bg: UInt32, contrast wanted: Double) -> UInt32 {
        for step in 0...100 {
            let c = mix(color, target, Double(step) / 100)
            if contrast(c, bg) >= wanted { return c }
        }
        return target
    }
}

/// Per sRGB channel, rounded, as Omarchy's `mix a b t` does.
private nonisolated func mix(_ a: UInt32, _ b: UInt32, _ t: Double) -> UInt32 {
    [16, 8, 0].reduce(0) { out, shift in
        let x = Double((a >> shift) & 0xFF), y = Double((b >> shift) & 0xFF)
        return out | UInt32((x * (1 - t) + y * t).rounded()) << shift
    }
}

/// WCAG 2 contrast ratio, 1 to 21.
private nonisolated func contrast(_ a: UInt32, _ b: UInt32) -> Double {
    func luminance(_ c: UInt32) -> Double {
        let linear = [16, 8, 0].map { shift -> Double in
            let v = Double((c >> shift) & 0xFF) / 255
            return v <= 0.04045 ? v / 12.92 : pow((v + 0.055) / 1.055, 2.4)
        }
        return 0.2126 * linear[0] + 0.7152 * linear[1] + 0.0722 * linear[2]
    }
    let (la, lb) = (luminance(a), luminance(b))
    return (max(la, lb) + 0.05) / (min(la, lb) + 0.05)
}
