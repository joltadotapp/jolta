import Foundation

/// The colors of the empty inbox for one theme: a sky with two hills for each
/// time of day, and the bolt above the words.
///
/// A dark theme keeps the sky dark all day, so an empty inbox never lights up
/// a dark screen: the hour only tints it, warm at dawn and dusk, a little
/// brighter and bluer by day, deepest at night. A light theme gets the whole
/// day, pale by day and dark at night, as Graphite Light was drawn.
nonisolated struct Scenery: Equatable, Sendable {
    nonisolated struct Sky: Equatable, Sendable {
        var top, bottom, far, near, ink: UInt32
    }

    var dawn, day, dusk, night: Sky
    /// The bolt's fill from its lit top to its deep tip, its edge and its glow.
    var boltTop, boltMiddle, boltTip, boltEdge, glow: UInt32
    var stars: UInt32
}

/// The named colors of a theme its sky is mixed from. They are whatever the
/// theme calls them: White's "blue" is a grey, so its sky is grey too.
nonisolated struct SceneryHues: Sendable {
    var blue, cyan, green, magenta, orange, yellow: UInt32

    /// The hues of Graphite Light's hand-drawn skies, for Graphite Dark's.
    static let graphite = SceneryHues(blue: 0x5FA8E8, cyan: 0x8FD0F0, green: 0x8DBF8A, magenta: 0x8E6AA8, orange: 0xF08A5D, yellow: 0xFFC233)
}

nonisolated extension Scenery {
    init(mode: ThemeMode, bg: UInt32, deep: UInt32, fg: UInt32, hues h: SceneryHues) {
        let (light, dark) = luminance(bg) > luminance(fg) ? (bg, fg) : (fg, bg)
        if mode == .dark {
            // Each hue is mixed only a little into the theme's own background.
            func sky(top: UInt32, _ t: Double, bottom: UInt32, _ b: Double, hill: UInt32, _ h: Double) -> Sky {
                let top = mix(deep, top, t), bottom = mix(bg, bottom, b)
                return Sky(top: top, bottom: bottom, far: mix(bg, hill, h), near: mix(deep, hill, h * 0.45),
                           ink: Self.ink(on: top, bottom, light: light, dark: dark))
            }
            dawn = sky(top: h.blue, 0.12, bottom: h.orange, 0.24, hill: h.magenta, 0.2)
            day = sky(top: h.blue, 0.22, bottom: h.cyan, 0.2, hill: h.green, 0.22)
            dusk = sky(top: h.magenta, 0.12, bottom: h.orange, 0.3, hill: h.magenta, 0.18)
            night = sky(top: h.blue, 0.04, bottom: h.blue, 0.13, hill: h.blue, 0.12)
        } else {
            // Mixed like Graphite Light's skies, from the theme's hues, its
            // background and its text color.
            func sky(top: UInt32, bottom: UInt32, far: UInt32, near: UInt32) -> Sky {
                Sky(top: top, bottom: bottom, far: far, near: near, ink: Self.ink(on: top, bottom, light: light, dark: dark))
            }
            dawn = sky(top: mix(h.blue, dark, 0.35), bottom: mix(h.orange, light, 0.45),
                       far: mix(h.magenta, dark, 0.4), near: mix(h.magenta, dark, 0.62))
            // A theme whose blue is near black (White) would get a daytime sky dark at the top.
            let blue = readable(h.blue, toward: light, on: light, contrast: 1, atMost: 3.2)
            day = sky(top: blue, bottom: mix(blue, light, 0.78), far: h.green, near: mix(h.green, dark, 0.28))
            dusk = sky(top: mix(h.blue, dark, 0.6), bottom: h.orange,
                       far: mix(h.magenta, dark, 0.45), near: mix(h.magenta, dark, 0.7))
            let night = mix(dark, 0x000000, 0.55)
            self.night = sky(top: mix(night, h.blue, 0.06), bottom: mix(night, h.blue, 0.3),
                             far: mix(night, h.blue, 0.18), near: mix(night, h.blue, 0.08))
        }
        // Some themes' yellow is a dim grey (Last Horizon); the bolt has to stand out from the sky.
        let yellow = readable(h.yellow, toward: light, on: mode == .dark ? bg : mix(dark, 0x000000, 0.55), contrast: 4)
        boltTop = mix(yellow, 0xFFFFFF, 0.35)
        boltMiddle = yellow
        boltTip = h.orange
        boltEdge = mix(h.orange, 0x000000, 0.3)
        glow = mix(yellow, 0xFFFFFF, 0.4)
        stars = light
    }

    /// The app icon's amber bolt, which Graphite keeps in both modes.
    var withGraphiteBolt: Scenery {
        var s = self
        (s.boltTop, s.boltMiddle, s.boltTip, s.boltEdge, s.glow) = (0xFFE066, 0xFFC020, 0xFF8A00, 0xC86A00, 0xFFDB73)
        return s
    }

    /// The skies Graphite Light was drawn with.
    static let graphiteLight = Scenery(
        dawn: Sky(top: 0x5B6EA8, bottom: 0xF2B48A, far: 0x6E6A8E, near: 0x48466A, ink: 0xFFFFFF),
        day: Sky(top: 0x5FA8E8, bottom: 0xCFE8F7, far: 0x8DBF8A, near: 0x5F9A63, ink: 0x17304A),
        dusk: Sky(top: 0x2E3A78, bottom: 0xF08A5D, far: 0x6B4F7A, near: 0x3B2E52, ink: 0xFFFFFF),
        night: Sky(top: 0x0B1026, bottom: 0x26305A, far: 0x1C2244, near: 0x121630, ink: 0xFFFFFF),
        boltTop: 0, boltMiddle: 0, boltTip: 0, boltEdge: 0, glow: 0, stars: 0xFFFFFF
    ).withGraphiteBolt

    /// The theme's light or dark text color, whichever reads better on the
    /// middle of the sky, where the words are, pushed toward white or black
    /// when neither reads well enough.
    private static func ink(on top: UInt32, _ bottom: UInt32, light: UInt32, dark: UInt32) -> UInt32 {
        let middle = mix(top, bottom, 0.5)
        let lighter = contrast(light, middle) >= contrast(dark, middle)
        return readable(lighter ? light : dark, toward: lighter ? 0xFFFFFF : 0x000000, on: middle, contrast: 4.5)
    }
}
