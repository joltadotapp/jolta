import AppKit

/// A timed event placed in a day column of the hour grid: minutes from the
/// day's midnight, and its share of the column after packing.
struct PlacedEvent: Identifiable {
    let event: CalEvent
    let day: Int
    let top: Double
    let bottom: Double
    let column: Int
    let columns: Int

    var id: String { event.id + "#\(day)" }
}

/// An event in the row above the hours, from its first to its last day on screen.
struct LaneEvent: Identifiable {
    let event: CalEvent
    let first: Int
    let last: Int
    let lane: Int

    var id: String { event.id }
}

enum CalendarLayout {
    static let hourHeight: CGFloat = 48
    static let gutter: CGFloat = 64
    /// Events shorter than this draw at this height, so packing treats them as this long.
    static let shortestMinutes = 20.0
    static let workStart = 9
    static let workEnd = 18

    /// Timed events of less than a day per day column, packed: overlapping
    /// events share the column's width, each taking the first free column
    /// of its group of overlaps.
    static func timed(_ events: [CalEvent], days: [CalendarDay]) -> [PlacedEvent] {
        var out: [PlacedEvent] = []
        for (d, day) in days.enumerated() {
            let length = min(day.end.timeIntervalSince(day.start) / 60, 24 * 60)
            let items: [(CalEvent, Double, Double)] = events.compactMap { e in
                guard let start = e.start, let end = e.end, !e.isLong, start < day.end, end > day.start else { return nil }
                let top = max(0, start.timeIntervalSince(day.start) / 60)
                let bottom = min(length, end.timeIntervalSince(day.start) / 60)
                return (e, top, max(bottom, min(length, top + shortestMinutes)))
            }
            .sorted { $0.1 != $1.1 ? $0.1 < $1.1 : $0.2 > $1.2 }
            var group: [(CalEvent, Double, Double, Int)] = []
            var columnEnds: [Double] = []
            var groupEnd = -1.0
            func close() {
                out += group.map { PlacedEvent(event: $0.0, day: d, top: $0.1, bottom: $0.2, column: $0.3, columns: columnEnds.count) }
                group = []
                columnEnds = []
            }
            for (e, top, bottom) in items {
                if top >= groupEnd { close() }
                let column = columnEnds.firstIndex { $0 <= top } ?? columnEnds.count
                if column == columnEnds.count { columnEnds.append(bottom) } else { columnEnds[column] = bottom }
                group.append((e, top, bottom, column))
                groupEnd = max(groupEnd, bottom)
            }
            close()
        }
        return out
    }

    /// All-day and day-long events in lanes, each in the first lane free across its days.
    static func lanes(_ events: [CalEvent], days: [CalendarDay]) -> [LaneEvent] {
        guard let firstKey = days.first?.key, let lastKey = days.last?.key else { return [] }
        let keys = days.map(\.key)
        var laneEnds: [Int] = []
        return events.filter(\.isLong).compactMap { e -> LaneEvent? in
            let (a, b) = e.dayKeys
            guard a <= lastKey, b >= firstKey else { return nil }
            let first = keys.firstIndex { $0 >= a } ?? 0
            let last = keys.lastIndex { $0 <= b } ?? keys.count - 1
            let lane = laneEnds.firstIndex { $0 < first } ?? laneEnds.count
            if lane == laneEnds.count { laneEnds.append(last) } else { laneEnds[lane] = last }
            return LaneEvent(event: e, first: first, last: last, lane: lane)
        }
    }

    /// `10:00–11:00`.
    static func range(_ e: CalEvent) -> String {
        guard let start = e.start, let end = e.end else { return "all day" }
        return "\(CalendarModel.clockTime(start))–\(CalendarModel.clockTime(end))"
    }

    /// The line under the title: the meeting's provider, else the place.
    static func subtitle(_ e: CalEvent) -> String {
        if let provider = e.record.meetingProvider, provider != .other { return provider.name }
        return e.record.location
    }
}

/// The surface a calendar view lies on: the window's background in calendar
/// mode, the sidebar's in the day panel beside mail. Fills, hollow boxes and
/// the off-hours shading are mixed from it.
enum CalendarSurface: Hashable, Sendable {
    case bg, side

    nonisolated var token: KeyPath<ThemeTokens, UInt32> & Sendable {
        switch self {
        case .bg: \.bg
        case .side: \.side
        }
    }

    var color: NSColor {
        switch self {
        case .bg: Theme.bg
        case .side: Theme.side
        }
    }
}

/// A calendar's color as each theme draws it. The server's color is moved
/// toward the theme's text until it reads against the surface, so a dark
/// blue stays visible on a dark theme; fills are that color mixed into the
/// surface. Everything else in the calendar uses the theme's own tokens.
enum CalendarColor {
    enum Use: Hashable { case bar, fill, allDay, text }

    private struct Key: Hashable {
        let hex: String?
        let use: Use
        let surface: CalendarSurface
    }

    private static var cache: [Key: NSColor] = [:]

    /// Hours outside working hours and weekends: halfway from the background to the sidebar.
    static let offHours = NSColor(name: nil) { appearance in
        let t = ThemeStore.tokens(appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? .dark : .light)
        return NSColor(hex: mix(t.bg, t.side, 0.5))
    }

    static func color(_ hex: String?, _ use: Use, on surface: CalendarSurface = .bg) -> NSColor {
        let key = Key(hex: hex, use: use, surface: surface)
        if let c = cache[key] { return c }
        let base = hex.flatMap { UInt32($0.trimmingCharacters(in: ["#"]), radix: 16) }
        let color = NSColor(name: nil) { appearance in
            let mode: ThemeMode = appearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua ? .dark : .light
            let t = ThemeStore.tokens(mode)
            let under = t[keyPath: surface.token]
            let bar = readable(base ?? t.text3, toward: t.text, on: under, contrast: 3)
            return switch use {
            case .bar: NSColor(hex: bar)
            case .fill: NSColor(hex: mix(under, bar, mode == .dark ? 0.2 : 0.16))
            case .allDay: NSColor(hex: mix(under, bar, mode == .dark ? 0.28 : 0.22))
            case .text: NSColor(hex: readable(bar, toward: t.text, on: under, contrast: 4.5))
            }
        }
        cache[key] = color
        return color
    }
}
