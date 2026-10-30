import AppKit

/// Design tokens. Placeholder values until the design direction is chosen.
@MainActor
enum Theme {
    static let rowHeight: CGFloat = 34
    static let senderWidth: CGFloat = 180
    static let accent = NSColor.controlAccentColor
    static let text = NSColor.labelColor
    static let secondaryText = NSColor.secondaryLabelColor
    static let tertiaryText = NSColor.tertiaryLabelColor
    static let selection = NSColor.selectedContentBackgroundColor.withAlphaComponent(0.25)
    static let monoFont = NSFont.monospacedDigitSystemFont(ofSize: 12, weight: .regular)

    static func font(weight: NSFont.Weight) -> NSFont {
        .systemFont(ofSize: 13, weight: weight)
    }

    private static let time: DateFormatter = {
        let f = DateFormatter()
        f.timeStyle = .short
        f.dateStyle = .none
        return f
    }()

    private static let dayMonth: DateFormatter = {
        let f = DateFormatter()
        f.setLocalizedDateFormatFromTemplate("d MMM")
        return f
    }()

    private static let full: DateFormatter = {
        let f = DateFormatter()
        f.dateStyle = .short
        f.timeStyle = .none
        return f
    }()

    static func shortDate(_ unix: Int64) -> String {
        let date = Date(timeIntervalSince1970: TimeInterval(unix))
        let cal = Calendar.current
        if cal.isDateInToday(date) { return time.string(from: date) }
        if cal.isDate(date, equalTo: .now, toGranularity: .year) { return dayMonth.string(from: date) }
        return full.string(from: date)
    }
}
