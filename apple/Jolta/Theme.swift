import AppKit
import SwiftUI

/// Design tokens of the theme on screen (ThemeStore). One accent, and it
/// only marks where the keyboard is.
@MainActor
enum Theme {
    static var bg: NSColor { ThemeStore.shared.color(\.bg) }
    static var side: NSColor { ThemeStore.shared.color(\.side) }
    static var reader: NSColor { ThemeStore.shared.color(\.reader) }
    static var raise: NSColor { ThemeStore.shared.color(\.raise) }
    static var line: NSColor { ThemeStore.shared.color(\.line) }
    static var line2: NSColor { ThemeStore.shared.color(\.line2) }
    static var text: NSColor { ThemeStore.shared.color(\.text) }
    static var text2: NSColor { ThemeStore.shared.color(\.text2) }
    static var text3: NSColor { ThemeStore.shared.color(\.text3) }
    static var fill: NSColor { ThemeStore.shared.color(\.fill) }
    static var hover: NSColor { ThemeStore.shared.color(\.hover) }
    static var accent: NSColor { ThemeStore.shared.color(\.accent) }
    static var star: NSColor { ThemeStore.shared.color(\.star) }
    static var accentText: NSColor { ThemeStore.shared.color(\.accentText) }
    static var kbd: NSColor { ThemeStore.shared.color(\.kbd) }
    static var kbdText: NSColor { ThemeStore.shared.color(\.kbdText) }

    static let rowHeight: CGFloat = 40
    static let senderWidth: CGFloat = 136
    static let sidebarWidth: CGFloat = 232
    static let listWidth: CGFloat = 520
    static let headerHeight: CGFloat = 52

    static func font(_ size: CGFloat = 13, weight: NSFont.Weight = .regular) -> NSFont {
        .systemFont(ofSize: size, weight: weight)
    }

    /// Digits line up; letters stay proportional so `26/10/24` fits the 52 pt date column.
    static let timeFont = NSFont.monospacedDigitSystemFont(ofSize: 11.5, weight: .regular)

    private static let time: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "HH:mm"
        return f
    }()

    private static let weekday: DateFormatter = {
        let f = DateFormatter()
        f.setLocalizedDateFormatFromTemplate("EEE")
        return f
    }()

    private static let dayMonth: DateFormatter = {
        let f = DateFormatter()
        f.setLocalizedDateFormatFromTemplate("d MMM")
        return f
    }()

    private static let full: DateFormatter = {
        let f = DateFormatter()
        f.setLocalizedDateFormatFromTemplate("ddMMyy")
        return f
    }()

    /// Today: `09:32`. This week: `Tue`. This year: `26 Oct`. Older: `26/10/24`, in the locale's order.
    static func shortDate(_ unix: Int64) -> String {
        let date = Date(timeIntervalSince1970: TimeInterval(unix))
        let cal = Calendar.current
        if cal.isDateInToday(date) { return time.string(from: date) }
        if let days = cal.dateComponents([.day], from: cal.startOfDay(for: date), to: cal.startOfDay(for: .now)).day, days < 7 {
            return weekday.string(from: date)
        }
        if cal.isDate(date, equalTo: .now, toGranularity: .year) { return dayMonth.string(from: date) }
        return full.string(from: date)
    }
}

extension NSColor {
    nonisolated convenience init(hex: UInt32, alpha: CGFloat = 1) {
        self.init(srgbRed: CGFloat((hex >> 16) & 0xFF) / 255, green: CGFloat((hex >> 8) & 0xFF) / 255,
                  blue: CGFloat(hex & 0xFF) / 255, alpha: alpha)
    }
}

extension Color {
    init(_ token: NSColor) { self.init(nsColor: token) }
}

/// A key hint such as `E` or `⌘K`. Accent outline when it belongs to the focused thing.
struct KeyCap: View {
    let key: String
    var focused = false

    var body: some View {
        Text(key)
            .font(.system(size: 10.5, weight: .medium))
            .foregroundStyle(Color(focused ? Theme.accentText : Theme.kbdText))
            .padding(.horizontal, 4)
            .frame(minWidth: 17, minHeight: 17)
            .background {
                RoundedRectangle(cornerRadius: 5)
                    .fill(focused ? Color.clear : Color(Theme.kbd))
                    .strokeBorder(focused ? Color(Theme.accent).opacity(0.55) : .clear, lineWidth: 1)
            }
    }
}
