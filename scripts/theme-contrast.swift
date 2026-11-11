// Checks every theme's colors against the contrast they need on every surface
// they are drawn on. Run it with scripts/theme-contrast.sh.

import Foundation

/// Text that has to be read: WCAG AA for small text.
let textRatio = 4.5
/// A mark that has to be seen: the keyboard cursor, a star.
let markRatio = 3.0

var failures: [String] = []

func check(_ theme: ThemePalette, _ what: String, _ a: UInt32, _ b: UInt32, atLeast wanted: Double) {
    let ratio = contrast(a, b)
    if ratio < wanted {
        failures.append("\(theme.name): \(what) is \((ratio * 100).rounded() / 100), needs \(wanted)")
    }
}

for theme in ThemePalette.all {
    let t = theme.tokens
    let plain: [(String, UInt32)] = [("bg", t.bg), ("side", t.side), ("reader", t.reader), ("raise", t.raise)]
    // A selected or hovered row can sit on any of them.
    let selected = plain.map { ("fill on \($0.0)", t.fill) }
    let hovered = plain.map { ("hover on \($0.0)", t.hover) }
    for (name, surface) in plain + selected + hovered {
        check(theme, "text on \(name)", t.text, surface, atLeast: textRatio)
        check(theme, "text2 on \(name)", t.text2, surface, atLeast: textRatio)
        check(theme, "text3 on \(name)", t.text3, surface, atLeast: textRatio)
        check(theme, "accentText on \(name)", t.accentText, surface, atLeast: textRatio)
        check(theme, "accent on \(name)", t.accent, surface, atLeast: markRatio)
        check(theme, "star on \(name)", t.star, surface, atLeast: markRatio)
    }
    for (name, surface) in plain {
        check(theme, "fill against \(name)", t.fill, surface, atLeast: 1.1)
        check(theme, "hover against \(name)", t.hover, surface, atLeast: 1.04)
        check(theme, "line against \(name)", t.line, surface, atLeast: 1.1)
    }
    check(theme, "line2 against bg", t.line2, t.bg, atLeast: 1.08)
    check(theme, "kbdText on kbd", t.kbdText, t.kbd, atLeast: textRatio)
    // The steps of the text scale stay apart, so secondary text still looks secondary.
    check(theme, "text against text3", t.text, t.text3, atLeast: 1.25)
}

if failures.isEmpty {
    print("\(ThemePalette.all.count) themes pass")
} else {
    failures.forEach { print($0) }
    print("\(failures.count) failures")
    exit(1)
}
