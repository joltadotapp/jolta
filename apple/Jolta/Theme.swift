import AppKit
import SwiftUI

/// Graphite design tokens (docs/design/direction-a.html). One accent, and it
/// only marks where the keyboard is.
@MainActor
enum Theme {
    static let bg = dynamic(dark: 0x121214, light: 0xFBFBFA)
    static let side = dynamic(dark: 0x0C0C0E, light: 0xF3F3F1)
    static let reader = dynamic(dark: 0x161619, light: 0xFFFFFF)
    static let raise = dynamic(dark: 0x1D1D21, light: 0xFFFFFF)
    static let line = dynamic(dark: 0x26262B, light: 0xE5E5E2)
    static let line2 = dynamic(dark: 0x1C1C20, light: 0xEFEFEC)
    static let text = dynamic(dark: 0xECECEE, light: 0x17171A)
    static let text2 = dynamic(dark: 0xA6A6AE, light: 0x55555D)
    static let text3 = dynamic(dark: 0x8D8D97, light: 0x66666E)
    static let fill = dynamic(dark: 0x202025, light: 0xEEEEEB)
    static let hover = dynamic(dark: 0x18181C, light: 0xF5F5F3)
    static let accent = dynamic(dark: 0xFF6B2C, light: 0xE5521A)
    static let accentText = dynamic(dark: 0xFF8A55, light: 0xB23A0A)
    static let kbd = dynamic(dark: 0x2A2A30, light: 0xECECEA)
    static let kbdText = dynamic(dark: 0xB4B4BC, light: 0x4A4A52)

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

    static func dynamic(dark: UInt32, light: UInt32) -> NSColor {
        NSColor(name: nil) { appearance in
            appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? NSColor(hex: dark) : NSColor(hex: light)
        }
    }

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
    convenience init(hex: UInt32, alpha: CGFloat = 1) {
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
