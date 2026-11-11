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
    let selected = plain.map { ("fill on \($0.0)", t.filled($0.1)) }
    let hovered = plain.map { ("hover on \($0.0)", t.hovered($0.1)) }
    for (name, surface) in plain + selected + hovered {
        check(theme, "text on \(name)", t.text, surface, atLeast: textRatio)
        check(theme, "text2 on \(name)", t.text2, surface, atLeast: textRatio)
        check(theme, "text3 on \(name)", t.text3, surface, atLeast: textRatio)
        check(theme, "accentText on \(name)", t.accentText, surface, atLeast: textRatio)
        check(theme, "accent on \(name)", t.accent, surface, atLeast: markRatio)
        check(theme, "star on \(name)", t.star, surface, atLeast: markRatio)
        check(theme, "dangerText on \(name)", t.dangerText, surface, atLeast: textRatio)
        check(theme, "warningText on \(name)", t.warningText, surface, atLeast: textRatio)
    }
    for (name, surface) in plain {
        check(theme, "fill against \(name)", t.filled(surface), surface, atLeast: 1.1)
        check(theme, "hover against \(name)", t.hovered(surface), surface, atLeast: 1.04)
        check(theme, "line against \(name)", t.line, surface, atLeast: 1.1)
    }
    check(theme, "line2 against bg", t.line2, t.bg, atLeast: 1.08)
    check(theme, "kbdText on kbd", t.kbdText, t.kbd, atLeast: textRatio)
    check(theme, "onAccent on accent", t.onAccent, t.accent, atLeast: textRatio)
    check(theme, "white on danger", 0xFFFFFF, t.danger, atLeast: textRatio)
    // The steps of the text scale stay apart, so secondary text still looks
    // secondary. Themes whose own text is about 7:1 (Catppuccin Latte and
    // Frappé, Rose Pine) leave little room above the 4.5 the dimmest text needs.
    check(theme, "text against text3", t.text, t.text3, atLeast: 1.15)
}

if CommandLine.arguments.contains("--print") {
    for theme in ThemePalette.all {
        let t = theme.tokens
        func hex(_ c: UInt32) -> String { String(format: "%06X", c) }
        print("\(theme.name): fill \(t.fillAlpha) hover \(t.hoverAlpha) text \(hex(t.text)) text2 \(hex(t.text2)) text3 \(hex(t.text3)) accent \(hex(t.accent)) accentText \(hex(t.accentText)) star \(hex(t.star)) line \(hex(t.line)) text:text3 \((contrast(t.text, t.text3) * 100).rounded() / 100)")
    }
}

if failures.isEmpty {
    print("\(ThemePalette.all.count) themes pass")
} else {
    failures.forEach { print($0) }
    print("\(failures.count) failures")
    exit(1)
}
