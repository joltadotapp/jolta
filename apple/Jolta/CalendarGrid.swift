import AppKit
import JoltaFFI
import QuartzCore
import SwiftUI

/// Everything the hour grid draws for one period.
struct HourGridContent {
    let placed: [PlacedEvent]
    /// Per day, its working hours as minutes after midnight; nil on a day off.
    let work: [Range<Int>?]
    /// The column of today, for the current-time line.
    let today: Int?
    let todayStart: Date?
    let selectedID: String?
    let now: Date
    let gutter: CGFloat
    /// Bumped with every new set of occurrences.
    let revision: Int
    /// The event the editor holds, drawn dashed where its fields put it.
    var draft: DraftBlock?
    /// An event moved on the grid that waits for a dialog or more keys, drawn like the draft.
    var pending: DraftBlock?
    /// The period's days, for the times a drag lands on.
    var columns: [CalendarDay] = []
    /// Calendars whose events show the resize cursor at their edges.
    var writable: Set<Int64> = []
    /// What the grid lies on.
    var surface = CalendarSurface.bg

    var days: Int { work.count }

    static let empty = HourGridContent(placed: [], work: [nil], today: nil, todayStart: nil, selectedID: nil, now: .now, gutter: 64, revision: -1)
}

/// A dashed event on one day of the period: the editor's draft, an event
/// waiting to be saved, or one being dragged.
struct DraftBlock: Hashable {
    let day: Int
    /// Minutes after the day's midnight.
    let top: Double
    let bottom: Double
    let title: String
    let range: String
    let color: String?
    /// Its real times, which `top` and `bottom` cut to the day.
    let start: Date
    let end: Date
}

extension DraftBlock {
    /// From `start` to `end` on the period's days, or nil when it starts elsewhere.
    init?(start: Date, end: Date, title: String, days: [CalendarDay], color: String?) {
        guard let day = days.firstIndex(where: { $0.start <= start && start < $0.end }) else { return nil }
        let top = start.timeIntervalSince(days[day].start) / 60
        self.init(day: day, top: top, bottom: min(max(end.timeIntervalSince(days[day].start) / 60, top + 15), 24 * 60), title: title,
                  range: "\(CalendarModel.clockTime(start)) – \(CalendarModel.clockTime(end))", color: color, start: start, end: end)
    }

    /// The editor's draft, or nil when it is all-day or elsewhere.
    @MainActor
    init?(_ editor: EventEditorModel, days: [CalendarDay], color: String?) {
        guard let (start, end) = editor.interval else { return nil }
        self.init(start: start, end: end, title: editor.draft.title.isEmpty ? "New event" : editor.draft.title, days: days, color: color)
    }
}

/// What the hour grid's drags are on: an event, or the editor's draft.
enum GridTarget {
    case event(CalEvent)
    case draft
}

/// A drag on the hour grid, as it is let go. Times are minutes after the
/// midnight of the period's day `day`.
enum GridDrop {
    /// Over empty time: a new event.
    case create(day: Int, top: Int, bottom: Int)
    /// ⌥ over empty time: focus time.
    case focus(day: Int, top: Int, bottom: Int)
    /// To start at `top`, keeping the length; with ⌥ on an event, a copy.
    case move(GridTarget, day: Int, top: Int, copy: Bool)
    /// A new start, or with `top` false a new end.
    case resize(GridTarget, top: Bool, day: Int, minute: Int)
}

/// A flipped, transparent view that draws itself with `paint`.
class CanvasView: NSView {
    override var isFlipped: Bool { true }
    override var isOpaque: Bool { false }

    func paint(_ cg: CGContext) {}

    override func draw(_ dirtyRect: NSRect) {
        guard let cg = NSGraphicsContext.current?.cgContext else { return }
        paint(cg)
    }
}

/// The look of one event box.
@MainActor
enum EventStyle {
    static let titleFont = NSFont.systemFont(ofSize: 11.5, weight: .semibold)
    static let timeFont = NSFont.monospacedSystemFont(ofSize: 10.5, weight: .medium)
    static let subFont = NSFont.systemFont(ofSize: 11)

    static func rect(_ p: PlacedEvent, width: CGFloat, days: Int) -> CGRect {
        let colW = width / CGFloat(max(days, 1))
        let share = colW / CGFloat(p.columns)
        let x = CGFloat(p.day) * colW + share * CGFloat(p.column) + (p.column == 0 ? 3 : 1)
        let w = share * CGFloat(p.span) - (p.column == 0 ? 3 : 1) - (p.column + p.span == p.columns ? 4 : 1)
        let y = p.top / 60 * CalendarLayout.hourHeight + 1
        let h = max(16, (p.bottom - p.top) / 60 * CalendarLayout.hourHeight - 2)
        return CGRect(x: x, y: y, width: max(w, 4), height: h)
    }

    /// How strongly the fill and bar of a past or declined event show. Its
    /// text keeps full strength in a quieter color (`ink`), so it still reads.
    static func alpha(_ e: CalEvent, now: Date) -> CGFloat {
        if e.isDeclined || e.isCancelled { return 0.45 }
        return e.isPast(now) ? 0.55 : 1
    }

    /// The accent ring around the box the keyboard acts on, `gap` points out from it.
    static func ring(_ r: CGRect, radius: CGFloat, gap: CGFloat = 1.5) {
        Theme.accent.setStroke()
        let ring = NSBezierPath(roundedRect: r.insetBy(dx: -gap, dy: -gap), xRadius: radius + gap, yRadius: radius + gap)
        ring.lineWidth = 2
        ring.stroke()
    }

    /// The colors of an event's title and of the lines under it.
    static func ink(_ e: CalEvent, now: Date) -> (title: NSColor, sub: NSColor) {
        if e.isDeclined || e.isCancelled { return (Theme.text3, Theme.text3) }
        return e.isPast(now) ? (Theme.text2, Theme.text3) : (Theme.text, Theme.text2)
    }

    private static let ascent = ceil(titleFont.ascender)
    private static let lineHeight: CGFloat = 14

    /// One line of text, cut to `width` with an ellipsis.
    private static func line(_ parts: [(String, NSFont)], width: CGFloat) -> CTLine {
        let text = NSMutableAttributedString()
        for (string, font) in parts {
            text.append(NSAttributedString(string: string, attributes: [.font: font, kCTForegroundColorFromContextAttributeName as NSAttributedString.Key: true]))
        }
        let line = CTLineCreateWithAttributedString(text)
        guard CTLineGetTypographicBounds(line, nil, nil, nil) > width else { return line }
        let dots = CTLineCreateWithAttributedString(NSAttributedString(string: "…", attributes: [.font: parts[0].1, kCTForegroundColorFromContextAttributeName as NSAttributedString.Key: true]))
        return CTLineCreateTruncatedLine(line, width, .end, dots) ?? line
    }

    private static let unsynced = NSImage(systemSymbolName: "arrow.triangle.2.circlepath", accessibilityDescription: "Not synced yet")?
        .withSymbolConfiguration(NSImage.SymbolConfiguration(pointSize: 8.5, weight: .bold).applying(NSImage.SymbolConfiguration(paletteColors: [Theme.text2])))

    private static let moon = NSImage(systemSymbolName: "moon.fill", accessibilityDescription: "Focus time")?
        .withSymbolConfiguration(NSImage.SymbolConfiguration(pointSize: 8.5, weight: .semibold).applying(NSImage.SymbolConfiguration(paletteColors: [Theme.text2])))

    /// An out-of-office band: the calendar's color at 45 percent, as Find a time draws it.
    static func awayFill(_ e: CalEvent, on surface: CalendarSurface) -> NSColor {
        CalendarColor.color(e.record.color, .bar, on: surface).withAlphaComponent(0.45)
    }

    /// The room the "not synced yet" mark takes at the box's right edge.
    static func markWidth(_ e: CalEvent) -> CGFloat {
        e.record.notSynced && unsynced != nil ? 13 : 0
    }

    /// The "not synced yet" mark in the box's top right corner, while an
    /// edit of the event waits to reach the server.
    static func drawMark(_ e: CalEvent, in r: CGRect) {
        guard e.record.notSynced, let image = unsynced, r.width >= 30 else { return }
        let size = image.size
        let frame = CGRect(x: r.maxX - size.width - 4, y: r.minY + max(3, min(4, (r.height - size.height) / 2)), width: size.width, height: size.height)
        image.draw(in: frame, from: .zero, operation: .sourceOver, fraction: 1, respectFlipped: true, hints: nil)
    }

    /// Title, times and place, top down, as many lines as the box holds. A
    /// box under 30 points tall gets the title and start on one line; one too
    /// narrow to read gets none.
    static func drawText(_ e: CalEvent, in r: CGRect, now: Date, cg: CGContext) {
        drawMark(e, in: r)
        let ink = ink(e, now: now)
        let width = r.width - 10 - markWidth(e)
        guard width >= 14 else { return }
        let x = r.minX + 7
        let struck = e.isDeclined || e.isCancelled
        var top = r.minY + (r.height < 30 ? 1 : 3)
        func draw(_ line: CTLine, _ color: NSColor) {
            cg.setFillColor(color.cgColor)
            cg.textPosition = CGPoint(x: x, y: top + ascent)
            CTLineDraw(line, cg)
            top += lineHeight
        }
        cg.textMatrix = CGAffineTransform(scaleX: 1, y: -1)
        let short = r.height < 30
        var title: [(String, NSFont)] = [(e.title, titleFont)]
        if short, let start = e.start { title.append(("  " + CalendarModel.clockTime(start), timeFont)) }
        let moonWidth: CGFloat = e.isFocus && moon != nil ? 13 : 0
        let titleLine = line(title, width: width - moonWidth)
        let titleWidth = CTLineGetTypographicBounds(titleLine, nil, nil, nil)
        if moonWidth > 0, let moon {
            let size = moon.size
            let frame = CGRect(x: x + titleWidth + 4, y: top + ascent - size.height + 1, width: size.width, height: size.height)
            moon.draw(in: frame, from: .zero, operation: .sourceOver, fraction: 1, respectFlipped: true, hints: nil)
        }
        draw(titleLine, ink.title)
        if struck {
            ink.title.setFill()
            CGRect(x: x, y: r.minY + (short ? 1 : 3) + ascent - 4, width: min(titleWidth, width), height: 1).fill()
        }
        guard !short else { return }
        if top + lineHeight <= r.maxY + 2 { draw(line([(CalendarLayout.range(e), timeFont)], width: width), ink.sub) }
        let sub = CalendarLayout.subtitle(e)
        if !sub.isEmpty, top + lineHeight <= r.maxY + 2 { draw(line([(sub, subFont)], width: width), ink.sub) }
    }
}

// MARK: - Hour grid

/// The day, work week and week views, and the day panel's hours, in
/// AppKit: day names and all-day lanes on top, the hour grid below. The
/// model updates it directly when the period or the selection changes, so
/// a key's new week draws in the same pass as the key instead of waiting
/// for SwiftUI's next update (measured at about 4 ms on a 120 Hz screen).
struct WeekGrid: NSViewRepresentable {
    let cal: CalendarModel
    let source: CalendarWeekView.Source
    var gutter = CalendarLayout.gutter
    /// The hour at the top when it first shows; nil puts now a third of the
    /// way down when today is among the days, and 07:30 on top otherwise.
    var firstHour: Double?
    var surface = CalendarSurface.bg
    var select: ((CalEvent) -> Void)?
    var open: ((CalEvent) -> Void)?
    /// The selected event's frame in this view, or nil when it isn't in sight.
    var selectedFrame: (CGRect?) -> Void = { _ in }

    func makeNSView(context: Context) -> CalendarWeekView {
        let view = CalendarWeekView(source: source, gutter: gutter, firstHour: firstHour, surface: surface)
        view.model = cal
        cal.attach(view)
        return view
    }

    /// Fills what it is offered without asking AppKit: measuring the scroll
    /// view ran its layout on every sibling's change and cost about 5 ms a switch.
    func sizeThatFits(_ proposal: ProposedViewSize, nsView: CalendarWeekView, context: Context) -> CGSize? {
        CGSize(width: proposal.width ?? 400, height: proposal.height ?? 400)
    }

    func updateNSView(_ view: CalendarWeekView, context: Context) {
        view.select = select ?? cal.select
        view.open = open ?? cal.openDetail
        view.swipe = { cal.shift($0) }
        view.selectedFrame = selectedFrame
        view.hours.canvas.create = source == .period ? { day, minute in
            if cal.days.indices.contains(day) { cal.newEvent(day: cal.days[day], minute: minute) }
        } : nil
        view.hours.canvas.drop = source == .period ? { cal.drop($0) } : nil
        view.hours.canvas.canMove = { cal.canMove($0) }
        view.strip.drop = source == .period ? { cal.move($0, days: $1, copy: $2) } : nil
        view.strip.canMove = { cal.canMove(.event($0)) }
        view.update()
    }
}

final class CalendarWeekView: NSView {
    enum Source {
        /// The period calendar mode shows, with day names.
        case period
        /// Today, for the panel beside mail.
        case today
    }

    let source: Source
    let surface: CalendarSurface
    weak var model: CalendarModel?
    let strip: DayStrip
    let hours = HourScrollView()
    var select: (CalEvent) -> Void = { _ in } { didSet { strip.select = select; hours.canvas.select = select } }
    var open: (CalEvent) -> Void = { _ in } { didSet { strip.open = open; hours.canvas.open = open } }
    var swipe: (Int) -> Void = { _ in } { didSet { hours.swipe = swipe } }
    var selectedFrame: (CGRect?) -> Void = { _ in }
    private var stripFrame: CGRect?
    private var gridFrame: CGRect?
    private var reported: CGRect??

    override var isFlipped: Bool { true }

    init(source: Source, gutter: CGFloat, firstHour: Double?, surface: CalendarSurface) {
        self.source = source
        self.surface = surface
        strip = DayStrip(gutter: gutter, showsNames: source == .period, showsEmptyLane: source == .period, surface: surface)
        super.init(frame: .zero)
        hours.firstHour = firstHour
        hours.canvas.gutter = gutter
        addSubview(hours)
        addSubview(strip)
        strip.selectedFrame = { [weak self] in self?.stripFrame = $0; self?.reportSelection() }
        strip.openDay = source == .period ? { [weak self] index in
            guard let cal = self?.model, cal.days.indices.contains(index) else { return }
            cal.openDay(cal.days[index])
        } : nil
        hours.canvas.selectedFrame = { [weak self] in self?.gridFrame = $0; self?.reportSelection() }
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    /// Reads the period, events and selection from the model.
    func update() {
        guard let cal = model else { return }
        CalendarBench.shared?.mark("grid update")
        var days: [CalendarDay], events: [CalEvent], revision: Int, selected: String?
        var draft: DraftBlock?, pending: DraftBlock?
        switch source {
        case .period:
            (days, events, revision, selected) = (cal.days, cal.events, cal.revision, cal.selectedID)
            if let editor = cal.editor, !editor.isReadOnly {
                // The event being edited shows only as the draft.
                if let editing = editor.editing { events = events.filter { $0.id != editing } }
                draft = DraftBlock(editor, days: days, color: cal.color(of: editor.shownCalendar))
                revision = revision &* 31 &+ (draft?.hashValue ?? 0) &+ (editor.editing?.hashValue ?? 0)
            }
            if let change = cal.gridChange {
                // A moved event shows only where it goes; a copy beside the original.
                if !change.copy { events = events.filter { $0.id != change.event.id } }
                let moved = change.preview
                if let start = moved.start, let end = moved.end, !moved.isLong {
                    pending = DraftBlock(start: start, end: end, title: moved.title, days: days, color: moved.record.color)
                } else {
                    events.append(moved)
                }
                revision = revision &* 31 &+ moved.id.hashValue &+ change.copy.hashValue &+ 1
            }
        case .today: (days, events, revision, selected) = (cal.today.map { [$0] } ?? [], cal.todayEvents, cal.todayRevision, nil)
        }
        guard !days.isEmpty else { return }
        let today = days.firstIndex(where: \.isToday)
        // One ring: the strip leaves it to a draft or an unsaved move in the hours.
        strip.content = DayStripContent(days: days, lanes: CalendarLayout.lanes(events, days: days),
                                        selectedID: draft == nil && pending == nil ? selected : nil, now: cal.now, revision: revision)
        hours.canvas.content = HourGridContent(placed: CalendarLayout.timed(events, days: days), work: days.map(cal.workSpan), today: today,
                                               todayStart: today.map { days[$0].start }, selectedID: selected, now: cal.now,
                                               gutter: hours.canvas.gutter, revision: revision, draft: draft, pending: pending,
                                               columns: days, writable: source == .period ? cal.writableCalendars : [], surface: surface)
        if strip.frame.height != strip.height { needsLayout = true }
        // Draw now, inside the key's own pass, rather than at the next
        // display pass, which waited about 7 ms for the frame on a 120 Hz screen.
        guard window != nil else { return }
        layoutSubtreeIfNeeded()
        strip.displayIfNeeded()
        hours.canvas.displayIfNeeded()
    }

    override func layout() {
        super.layout()
        let h = strip.height
        strip.frame = CGRect(x: 0, y: 0, width: bounds.width, height: h)
        hours.frame = CGRect(x: 0, y: h, width: bounds.width, height: max(0, bounds.height - h))
    }

    /// Esc: a drag under way ends and its event stays where it was. False when nothing was being dragged.
    func cancelDrag() -> Bool {
        let strip = strip.cancelDrag(), hours = hours.canvas.cancelDrag()
        return strip || hours
    }

    /// T: the hours scroll to now, when today is among the days.
    func revealNow() {
        if source == .period { hours.scrollToNow() }
    }

    /// Scrolls the hours to the selected event when it is a timed one out of sight.
    func revealSelection() {
        guard source == .period else { return }
        let content = hours.canvas.content
        let parts = content.placed.filter { $0.event.id == content.selectedID }
        guard let top = parts.map(\.top).min(), let bottom = parts.map(\.bottom).max() else { return }
        hours.reveal(from: top / 60 * CalendarLayout.hourHeight, to: bottom / 60 * CalendarLayout.hourHeight)
    }

    private func reportSelection() {
        let frame = stripFrame.map { convert($0, from: strip) } ?? gridFrame.map { convert($0, from: hours.canvas) }
        guard reported != .some(frame) else { return }
        reported = .some(frame)
        selectedFrame(frame)
    }
}

/// What the strip above the hours draws.
struct DayStripContent {
    let days: [CalendarDay]
    let lanes: [LaneEvent]
    let selectedID: String?
    let now: Date
    let revision: Int

    static let empty = DayStripContent(days: [], lanes: [], selectedID: nil, now: .now, revision: -1)
}

/// Day names, then all-day and multi-day events in lanes: up to three above
/// several days, with "+N" under a day that has more, and all of them above
/// a single day.
final class DayStrip: CanvasView {
    let gutter: CGFloat
    let showsNames: Bool
    let showsEmptyLane: Bool
    let surface: CalendarSurface
    var content = DayStripContent.empty {
        didSet {
            guard content.revision != oldValue.revision || content.selectedID != oldValue.selectedID
                    || content.now != oldValue.now || content.days != oldValue.days else { return }
            elements = nil
            more = content.days.indices.map { day in content.lanes.filter { $0.lane >= maxLanes && $0.first <= day && day <= $0.last }.count }
            needsDisplay = true
            reportSelection()
            window?.invalidateCursorRects(for: self)
        }
    }
    var select: (CalEvent) -> Void = { _ in }
    var open: (CalEvent) -> Void = { _ in }
    var selectedFrame: (CGRect?) -> Void = { _ in }
    /// A click on a day's "+N": the day view of that day.
    var openDay: ((Int) -> Void)?
    /// Per day, the events that have no lane.
    private var more: [Int] = []
    /// An event dragged to other days: by how many, and whether ⌥ copied it.
    var drop: ((CalEvent, Int, Bool) -> Void)?
    var canMove: (CalEvent) -> Bool = { _ in false }
    private var elements: [EventElement]?
    private var lastReported: CGRect??
    private var press: Press?

    /// A press on an event, until the mouse is let go.
    private struct Press {
        let item: LaneEvent
        let point: CGPoint
        let column: Int
        var moving = false
        var days = 0
        var copy = false
        var refused = false

        /// Where the event goes, when it is drawn there.
        func moved(_ columns: Int) -> LaneEvent? {
            guard moving, !refused else { return nil }
            let first = max(0, item.first + days), last = min(columns - 1, item.last + days)
            return first <= last ? LaneEvent(event: item.event, first: first, last: last, lane: item.lane) : nil
        }
    }

    private static let namesHeight: CGFloat = 36
    private static let laneHeight: CGFloat = 20
    private static let weekdayFont = NSFont.systemFont(ofSize: 12)
    private static let dayFont = NSFont.monospacedDigitSystemFont(ofSize: 17, weight: .semibold)
    private static let laneFont = NSFont.systemFont(ofSize: 11.5)
    private static let labelFont = NSFont.monospacedSystemFont(ofSize: 10.5, weight: .medium)

    init(gutter: CGFloat, showsNames: Bool, showsEmptyLane: Bool, surface: CalendarSurface) {
        self.gutter = gutter
        self.showsNames = showsNames
        self.showsEmptyLane = showsEmptyLane
        self.surface = surface
        super.init(frame: .zero)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    private var maxLanes: Int { content.days.count == 1 ? .max : CalendarLayout.maxLanes }
    private var laneCount: Int { min(maxLanes, (content.lanes.map(\.lane).max() ?? -1) + 1) }
    private var lanesTop: CGFloat { showsNames ? Self.namesHeight + 1 : 0 }

    var height: CGFloat {
        // The "+N" cells take a row of their own under the lanes.
        let rows = laneCount + (more.contains { $0 > 0 } ? 1 : 0)
        let lanes = rows == 0 && !showsEmptyLane ? 0 : max(1, rows)
        let laneArea = lanes == 0 ? 0 : 4 + CGFloat(lanes) * (Self.laneHeight + 3) - 3 + 6 + 1
        return lanesTop + laneArea
    }

    private var columnWidth: CGFloat { (bounds.width - gutter) / CGFloat(max(content.days.count, 1)) }

    private func box(_ item: LaneEvent) -> CGRect {
        CGRect(x: gutter + columnWidth * CGFloat(item.first) + 3, y: lanesTop + 4 + CGFloat(item.lane) * (Self.laneHeight + 3),
               width: columnWidth * CGFloat(item.last - item.first + 1) - 6, height: Self.laneHeight)
    }

    /// Where a day says "+N" for its events without a lane.
    private func moreBox(_ day: Int) -> CGRect {
        CGRect(x: gutter + columnWidth * CGFloat(day) + 3, y: lanesTop + 4 + CGFloat(laneCount) * (Self.laneHeight + 3),
               width: columnWidth - 6, height: Self.laneHeight)
    }

    private func moreDay(at point: CGPoint) -> Int? {
        more.indices.first { more[$0] > 0 && moreBox($0).contains(point) }
    }

    /// The selected event when it has no lane: its day's "+N" stands for it.
    private var selectedBehindMore: LaneEvent? {
        content.lanes.first { $0.lane >= maxLanes && $0.event.id == content.selectedID }
    }

    private static let weekday: DateFormatter = {
        let f = DateFormatter()
        f.setLocalizedDateFormatFromTemplate("EEE")
        return f
    }()

    private func line(_ text: String, _ font: NSFont) -> CTLine {
        CTLineCreateWithAttributedString(NSAttributedString(string: text, attributes: [.font: font, kCTForegroundColorFromContextAttributeName as NSAttributedString.Key: true]))
    }

    override func paint(_ cg: CGContext) {
        guard !content.days.isEmpty else { return }
        cg.textMatrix = CGAffineTransform(scaleX: 1, y: -1)
        let colW = columnWidth
        if showsNames {
            for (i, day) in content.days.enumerated() {
                var x = gutter + CGFloat(i) * colW + 10
                let name = line(Self.weekday.string(from: day.start), Self.weekdayFont)
                cg.setFillColor((day.isToday ? Theme.text : Theme.text3).cgColor)
                cg.textPosition = CGPoint(x: x, y: 26)
                CTLineDraw(name, cg)
                x += CTLineGetTypographicBounds(name, nil, nil, nil) + 6
                cg.setFillColor((day.isToday ? Theme.accentText : Theme.text2).cgColor)
                cg.textPosition = CGPoint(x: x, y: 26)
                CTLineDraw(line("\(Calendar.current.component(.day, from: day.start))", Self.dayFont), cg)
            }
            Theme.line2.setFill()
            CGRect(x: 0, y: Self.namesHeight, width: bounds.width, height: 1).fill()
        }
        guard height > lanesTop else { return }
        let label = line("all day", Self.labelFont)
        let labelWidth = CTLineGetTypographicBounds(label, nil, nil, nil)
        if labelWidth + 14 <= gutter {
            cg.setFillColor(Theme.text3.cgColor)
            cg.textPosition = CGPoint(x: gutter - 10 - labelWidth, y: lanesTop + 15)
            CTLineDraw(label, cg)
        }
        let moved = press?.moved(content.days.count)
        var ringed = selectedBehindMore.map { moreBox($0.first) }
        for (day, count) in more.enumerated() where count > 0 {
            let r = moreBox(day)
            cg.setFillColor(Theme.text2.cgColor)
            cg.textPosition = CGPoint(x: r.minX + 7, y: r.minY + 14)
            CTLineDraw(line("+\(count)", Self.laneFont), cg)
        }
        for item in content.lanes where item.lane < maxLanes {
            if moved != nil, press?.copy == false, item.id == moved?.id { continue }
            let e = item.event, r = box(item)
            if e.id == content.selectedID { ringed = r }
            let alpha = EventStyle.alpha(e, now: content.now)
            let ink = EventStyle.ink(e, now: content.now)
            if let place = e.place {
                drawPlace(place, item: item, alpha: alpha, ink: ink.sub, cg: cg)
                continue
            }
            cg.saveGState()
            if alpha < 1 { cg.setAlpha(alpha) }
            if e.isUnanswered {
                // Hollow until answered, as in the hours below.
                surface.color.setFill()
                NSBezierPath(roundedRect: r, xRadius: 5, yRadius: 5).fill()
                let edge = NSBezierPath(roundedRect: r.insetBy(dx: 0.5, dy: 0.5), xRadius: 4.5, yRadius: 4.5)
                CalendarColor.color(e.record.color, .bar, on: surface).setStroke()
                edge.stroke()
            } else {
                (e.isAway ? EventStyle.awayFill(e, on: surface) : CalendarColor.color(e.record.color, .allDay, on: surface)).setFill()
                NSBezierPath(roundedRect: r, xRadius: 5, yRadius: 5).fill()
            }
            cg.restoreGState()
            EventStyle.drawMark(e, in: r)
            var title = line(e.title, Self.laneFont)
            let width = r.width - 14 - EventStyle.markWidth(e)
            if CTLineGetTypographicBounds(title, nil, nil, nil) > width {
                title = CTLineCreateTruncatedLine(title, max(0, width), .end, line("…", Self.laneFont)) ?? title
            }
            cg.setFillColor(ink.title.cgColor)
            cg.textPosition = CGPoint(x: r.minX + 7, y: r.minY + 14)
            CTLineDraw(title, cg)
            if e.isDeclined || e.isCancelled {
                ink.title.setFill()
                CGRect(x: r.minX + 7, y: r.midY, width: min(CTLineGetTypographicBounds(title, nil, nil, nil), width), height: 1).fill()
            }
        }
        // After every event, so none drawn later covers it.
        if let ringed { EventStyle.ring(ringed, radius: 5) }
        if let moved { drawMoved(moved, cg: cg) }
        Theme.line2.setFill()
        CGRect(x: 0, y: bounds.height - 1, width: bounds.width, height: 1).fill()
    }

    private static let placeFont = NSFont.systemFont(ofSize: 11, weight: .medium)

    /// A working location: no box, its place with a glyph in each of its days.
    private func drawPlace(_ place: (label: String, at: Workplace), item: LaneEvent, alpha: CGFloat, ink: NSColor, cg: CGContext) {
        let symbol = switch place.at {
        case .home: "house"
        case .office: "building.2"
        case .other: "mappin.and.ellipse"
        }
        let glyph = NSImage(systemSymbolName: symbol, accessibilityDescription: nil)?
            .withSymbolConfiguration(NSImage.SymbolConfiguration(pointSize: 9.5, weight: .medium).applying(NSImage.SymbolConfiguration(paletteColors: [CalendarColor.color(item.event.record.color, .text, on: surface)])))
        let r = box(item)
        for day in item.first...item.last {
            let cell = CGRect(x: gutter + columnWidth * CGFloat(day) + 3, y: r.minY, width: columnWidth - 6, height: r.height)
            var x = cell.minX + 5
            if let glyph {
                glyph.draw(in: CGRect(x: x, y: cell.midY - glyph.size.height / 2, width: glyph.size.width, height: glyph.size.height),
                           from: .zero, operation: .sourceOver, fraction: alpha, respectFlipped: true, hints: nil)
                x += glyph.size.width + 4
            }
            var text = line(place.label, Self.placeFont)
            let width = cell.maxX - x - 4
            if CTLineGetTypographicBounds(text, nil, nil, nil) > width {
                text = CTLineCreateTruncatedLine(text, max(0, width), .end, line("…", Self.placeFont)) ?? text
            }
            cg.setFillColor(ink.cgColor)
            cg.textPosition = CGPoint(x: x, y: r.minY + 14)
            CTLineDraw(text, cg)
        }
    }

    /// The event being dragged, dashed on the days it would move to.
    private func drawMoved(_ item: LaneEvent, cg: CGContext) {
        let r = box(item)
        let bar = CalendarColor.color(item.event.record.color, .bar, on: surface)
        let shape = NSBezierPath(roundedRect: r, xRadius: 5, yRadius: 5)
        surface.color.setFill()
        shape.fill()
        bar.withAlphaComponent(0.12).setFill()
        shape.fill()
        let edge = NSBezierPath(roundedRect: r.insetBy(dx: 0.75, dy: 0.75), xRadius: 4.5, yRadius: 4.5)
        edge.lineWidth = 1.5
        edge.setLineDash([4, 3], count: 2, phase: 0)
        bar.withAlphaComponent(0.85).setStroke()
        edge.stroke()
        var title = line(item.event.title, Self.laneFont)
        let width = r.width - 14
        if CTLineGetTypographicBounds(title, nil, nil, nil) > width {
            title = CTLineCreateTruncatedLine(title, max(0, width), .end, line("…", Self.laneFont)) ?? title
        }
        cg.setFillColor(Theme.text.cgColor)
        cg.textPosition = CGPoint(x: r.minX + 7, y: r.minY + 14)
        CTLineDraw(title, cg)
    }

    func reportSelection() {
        let frame = content.lanes.first { $0.event.id == content.selectedID }.map { $0.lane < maxLanes ? box($0) : moreBox($0.first) }
        guard lastReported != .some(frame) else { return }
        lastReported = .some(frame)
        let report = selectedFrame
        DispatchQueue.main.async { report(frame) }
    }

    private func column(at x: CGFloat) -> Int {
        min(max(Int(floor((x - gutter) / columnWidth)), 0), max(content.days.count - 1, 0))
    }

    override func mouseDown(with event: NSEvent) {
        let point = convert(event.locationInWindow, from: nil)
        if let day = moreDay(at: point), let openDay { return openDay(day) }
        guard let hit = content.lanes.last(where: { $0.lane < maxLanes && box($0).contains(point) }) else { return super.mouseDown(with: event) }
        if event.clickCount == 2 { return open(hit.event) }
        select(hit.event)
        if drop != nil { press = Press(item: hit, point: point, column: column(at: point.x)) }
    }

    /// Dragging an event sideways moves it by whole days; ⌥ copies it.
    override func mouseDragged(with event: NSEvent) {
        guard var press else { return }
        let point = convert(event.locationInWindow, from: nil)
        if !press.moving {
            guard hypot(point.x - press.point.x, point.y - press.point.y) >= 4 else { return }
            press.moving = true
            press.refused = !canMove(press.item.event)
        }
        press.days = column(at: point.x) - press.column
        press.copy = event.modifierFlags.contains(.option)
        (press.refused ? NSCursor.operationNotAllowed : press.copy ? NSCursor.dragCopy : NSCursor.closedHand).set()
        self.press = press
        needsDisplay = true
    }

    override func mouseUp(with event: NSEvent) {
        guard let press else { return }
        self.press = nil
        guard press.moving else { return }
        NSCursor.arrow.set()
        needsDisplay = true
        // A refused move still goes to the model, which says why.
        if press.days != 0 { drop?(press.item.event, press.days, event.modifierFlags.contains(.option)) }
    }

    /// Without the press, the rest of the drag and its mouse-up do nothing.
    func cancelDrag() -> Bool {
        guard press?.moving == true else { return false }
        press = nil
        NSCursor.arrow.set()
        needsDisplay = true
        return true
    }

    override func resetCursorRects() {
        guard openDay != nil else { return }
        for day in more.indices where more[day] > 0 { addCursorRect(moreBox(day), cursor: .pointingHand) }
    }

    override func isAccessibilityElement() -> Bool { false }
    override func accessibilityRole() -> NSAccessibility.Role? { .group }
    override func accessibilityLabel() -> String? { "All-day events" }

    override func accessibilityChildren() -> [Any]? {
        if elements == nil {
            var list = content.lanes.filter { $0.lane < maxLanes }.map { item in
                let event = item.event, rect = box(item)
                return EventElement(label: event.accessibilityLabel, identifier: "event-\(event.title)",
                                    place: { [weak self] in self?.screenRect(rect) ?? .zero }) { [weak self] in self?.open(event) }
            }
            let behind = selectedBehindMore
            for day in more.indices where more[day] > 0 {
                let rect = moreBox(day), count = more[day]
                list.append(EventElement(label: "\(count) more all-day \(count == 1 ? "event" : "events"), \(CalendarModel.spokenDay(content.days[day].key))",
                                         identifier: "more-\(content.days[day].key)",
                                         value: behind.map { $0.first <= day && day <= $0.last } == true ? "selected" : nil,
                                         place: { [weak self] in self?.screenRect(rect) ?? .zero }) { [weak self] in self?.openDay?(day) })
            }
            for element in list { element.setAccessibilityParent(self) }
            elements = list
        }
        return elements
    }
}

final class HourScrollView: NSScrollView {
    let canvas = HourCanvas()
    private let document = Document()
    var firstHour: Double?
    var swipe: (Int) -> Void = { _ in }
    private var placedFirstHour = false
    private var swiped = 0.0

    /// A day tall and drawing nothing; only its size scrolls.
    final class Document: NSView {
        override var isFlipped: Bool { true }
    }

    init() {
        super.init(frame: .zero)
        drawsBackground = false
        hasVerticalScroller = true
        autohidesScrollers = true
        scrollerStyle = .overlay
        verticalScrollElasticity = .none
        documentView = document
        document.addSubview(canvas)
        contentView.postsBoundsChangedNotifications = true
        NotificationCenter.default.addObserver(self, selector: #selector(scrolled), name: NSView.boundsDidChangeNotification, object: contentView)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override func layout() {
        super.layout()
        let size = NSSize(width: contentView.bounds.width, height: 24 * CalendarLayout.hourHeight)
        if document.frame.size != size { document.frame.size = size }
        if !placedFirstHour, contentView.bounds.height > 0 {
            placedFirstHour = true
            if let firstHour {
                scroll(to: firstHour * CalendarLayout.hourHeight)
            } else if !scrollToNow() {
                scroll(to: 7.5 * CalendarLayout.hourHeight)
            }
        }
        follow()
    }

    /// Puts the current time a third of the way down. False when today isn't among the days.
    @discardableResult func scrollToNow() -> Bool {
        guard canvas.content.today != nil, let start = canvas.content.todayStart else { return false }
        let now = canvas.content.now.timeIntervalSince(start) / 3600 * CalendarLayout.hourHeight
        scroll(to: now - contentView.bounds.height / 3)
        return true
    }

    @objc private func scrolled() { follow() }

    private func scroll(to y: CGFloat) {
        let top = max(0, min(y, document.frame.height - contentView.bounds.height))
        guard abs(top - contentView.bounds.minY) > 0.5 else { return }
        contentView.scroll(to: NSPoint(x: 0, y: top))
        reflectScrolledClipView(contentView)
    }

    /// Scrolls as little as it takes to show the day from `top` to `bottom`
    /// (points from midnight) with some room around it; something taller
    /// than the view shows its start.
    func reveal(from top: CGFloat, to bottom: CGFloat) {
        let visible = contentView.bounds, room: CGFloat = 24
        guard visible.height > 0 else { return }
        if top - room < visible.minY {
            scroll(to: top - room)
        } else if bottom + room > visible.maxY {
            scroll(to: min(top - room, bottom + room - visible.height))
        }
    }

    /// Keeps the canvas over the visible part of the day.
    private func follow() {
        let visible = contentView.bounds
        if canvas.frame != visible {
            canvas.frame = visible
            canvas.needsDisplay = true
            canvas.reportSelection()
            window?.invalidateCursorRects(for: canvas)
        }
    }

    /// A two-finger swipe sideways changes the period; vertical scrolling scrolls.
    override func scrollWheel(with event: NSEvent) {
        guard event.hasPreciseScrollingDeltas, abs(event.scrollingDeltaX) > abs(event.scrollingDeltaY) || swiped != 0 else {
            return super.scrollWheel(with: event)
        }
        if event.phase == .began { swiped = 0 }
        if swiped.isFinite { swiped += event.scrollingDeltaX }
        if abs(swiped) > 80 {
            swipe(swiped > 0 ? -1 : 1)
            swiped = .infinity
        }
        if event.phase == .ended || event.phase == .cancelled || event.phase.isEmpty && event.momentumPhase.isEmpty { swiped = 0 }
    }
}

/// Draws the visible hours. Its frame is the visible part of the day in the
/// document's coordinates, so drawing shifts up by the frame's top.
final class HourCanvas: CanvasView {
    var content = HourGridContent.empty {
        didSet {
            guard content.revision != oldValue.revision || content.selectedID != oldValue.selectedID || content.draft != oldValue.draft
                    || content.pending != oldValue.pending || content.work != oldValue.work || content.now != oldValue.now || content.today != oldValue.today else { return }
            elements = nil
            needsDisplay = true
            reportSelection()
            window?.invalidateCursorRects(for: self)
        }
    }
    var select: (CalEvent) -> Void = { _ in }
    var open: (CalEvent) -> Void = { _ in }
    /// Double-click on empty time: the day's index and the minute clicked.
    var create: ((Int, Int) -> Void)?
    /// A drag let go; nil where the grid only shows, as beside mail.
    var drop: ((GridDrop) -> Void)?
    var canMove: (GridTarget) -> Bool = { _ in false }
    var selectedFrame: (CGRect?) -> Void = { _ in }
    var gutter = CalendarLayout.gutter
    private var elements: [EventElement]?
    private var lastReported: CGRect??
    private var press: Press?

    /// A press on an event, the editor's draft or empty time (no item),
    /// until the mouse is let go.
    private struct Press {
        enum Grip { case body, top, bottom }
        let item: GridTarget?
        let grip: Grip
        /// Where it went down, in the document's coordinates, and the day there.
        let point: CGPoint
        let day: Int
        /// The item's times; the press's own for empty time.
        let start: Date
        let end: Date
        let title: String
        let color: String?
        var moving = false
        var refused = false
        var copy = false
        /// ⌥ on empty time: the drag draws focus time.
        var focus = false
        var drop: GridDrop?
        var preview: DraftBlock?

        /// The event that shows only where it is dragged to.
        var hides: String? {
            guard moving, !refused, !copy, preview != nil, case .event(let e) = item else { return nil }
            return e.id
        }

        var hidesDraft: Bool {
            guard moving, !refused, preview != nil, case .draft = item else { return false }
            return true
        }
    }

    private var columnWidth: CGFloat { (bounds.width - content.gutter) / CGFloat(max(content.days, 1)) }

    /// An event's box in the document's coordinates.
    private func box(_ p: PlacedEvent) -> CGRect {
        EventStyle.rect(p, width: bounds.width - content.gutter, days: content.days).offsetBy(dx: content.gutter, dy: 0)
    }

    private static let labelFont = NSFont.monospacedSystemFont(ofSize: 10.5, weight: .medium)
    private static let nowFont = NSFont.monospacedSystemFont(ofSize: 10.5, weight: .semibold)

    private enum Ring { case preview, draft, pending, selection }

    override func paint(_ cg: CGContext) {
        CalendarBench.shared?.mark("draw start")
        let paint = {
            cg.translateBy(x: 0, y: -self.frame.minY)
            let visible = self.frame
            self.drawGrid(visible, cg: cg)
            let content = self.content, press = self.press
            let hidden = press?.hides
            var selected: [CGRect] = []
            for p in content.placed where p.event.id != hidden {
                let r = self.box(p)
                guard r.intersects(visible) else { continue }
                self.draw(p.event, in: r, cg: cg)
                if p.event.id == content.selectedID { selected.append(r) }
            }
            // One accent ring, on what the keyboard acts on: the block being
            // dragged when it is the editor's draft or the selected event,
            // else the editor's draft, else an event moved and not saved yet,
            // else the selection. It is drawn after every event, so a
            // neighbour drawn later can't cover it.
            let ring: Ring = if press?.hidesDraft == true || (hidden != nil && hidden == content.selectedID) { .preview }
                else if content.draft != nil { .draft }
                else if content.pending != nil { .pending }
                else { .selection }
            if ring == .selection { for r in selected { EventStyle.ring(r, radius: 6) } }
            if let draft = content.draft, press?.hidesDraft != true { self.drawDraft(draft, ringed: ring == .draft, cg: cg) }
            if let pending = content.pending { self.drawDraft(pending, ringed: ring == .pending, cg: cg) }
            if let press, !press.refused, let preview = press.preview { self.drawDraft(preview, ringed: ring == .preview, cg: cg) }
            self.drawNow(cg: cg)
        }
        if let bench = CalendarBench.shared { bench.time("draw (hour grid)", paint) } else { paint() }
        CalendarBench.shared?.drew()
    }

    private func drawGrid(_ visible: CGRect, cg: CGContext) {
        let hh = CalendarLayout.hourHeight
        let colW = columnWidth, x0 = content.gutter
        let height = 24 * hh
        CalendarColor.offHours(on: content.surface).setFill()
        for (i, work) in content.work.enumerated() {
            let x = x0 + CGFloat(i) * colW
            if let work {
                CGRect(x: x, y: 0, width: colW, height: CGFloat(work.lowerBound) / 60 * hh).fill()
                let end = CGFloat(work.upperBound) / 60 * hh
                CGRect(x: x, y: end, width: colW, height: height - end).fill()
            } else {
                CGRect(x: x, y: 0, width: colW, height: height).fill()
            }
        }
        Theme.line2.setFill()
        for i in 0..<content.days {
            CGRect(x: x0 + CGFloat(i) * colW, y: visible.minY, width: 1, height: visible.height).fill()
        }
        cg.textMatrix = CGAffineTransform(scaleX: 1, y: -1)
        let first = max(1, Int(visible.minY / hh)), last = min(23, Int(visible.maxY / hh) + 1)
        guard first <= last else { return }
        for h in first...last {
            let y = CGFloat(h) * hh
            Theme.line2.setFill()
            CGRect(x: x0, y: y, width: bounds.width - x0, height: 1).fill()
            drawLabel(String(format: "%02d:00", h), font: Self.labelFont, color: Theme.text3, y: y, cg: cg)
        }
    }

    /// A time right-aligned in the gutter, centred on `y`.
    private func drawLabel(_ text: String, font: NSFont, color: NSColor, y: CGFloat, background: NSColor? = nil, cg: CGContext) {
        let line = CTLineCreateWithAttributedString(NSAttributedString(string: text, attributes: [.font: font, kCTForegroundColorFromContextAttributeName as NSAttributedString.Key: true]))
        let width = CTLineGetTypographicBounds(line, nil, nil, nil)
        let x = content.gutter - 10 - width
        if let background {
            background.setFill()
            CGRect(x: x - 4, y: y - 8, width: width + 8, height: 16).fill()
        }
        cg.setFillColor(color.cgColor)
        cg.textPosition = CGPoint(x: x, y: y + 4)
        CTLineDraw(line, cg)
    }

    private func drawNow(cg: CGContext) {
        guard let today = content.today, let start = content.todayStart else { return }
        let y = content.now.timeIntervalSince(start) / 3600 * CalendarLayout.hourHeight
        let x = content.gutter + CGFloat(today) * columnWidth
        Theme.accent.setFill()
        CGRect(x: x, y: y - 0.75, width: columnWidth, height: 1.5).fill()
        NSBezierPath(ovalIn: CGRect(x: x - 3.5, y: y - 3.5, width: 7, height: 7)).fill()
        drawLabel(CalendarModel.clockTime(content.now), font: Self.nowFont, color: Theme.accentText, y: y, background: content.surface.color, cg: cg)
    }

    private func draw(_ e: CalEvent, in r: CGRect, cg: CGContext) {
        let alpha = EventStyle.alpha(e, now: content.now)
        cg.saveGState()
        if alpha < 1 { cg.setAlpha(alpha) }
        let box = NSBezierPath(roundedRect: r, xRadius: 6, yRadius: 6)
        let surface = content.surface
        let bar = CalendarColor.color(e.record.color, .bar, on: surface)
        if e.isUnanswered || e.isFree {
            // Hollow: time that isn't taken. An invitation still to answer
            // keeps its color bar; an event shown as free is an outline alone.
            surface.color.setFill()
            box.fill()
            let width: CGFloat = e.isUnanswered ? 1 : 1.5
            let edge = NSBezierPath(roundedRect: r.insetBy(dx: width / 2, dy: width / 2), xRadius: 6 - width / 2, yRadius: 6 - width / 2)
            edge.lineWidth = width
            bar.setStroke()
            edge.stroke()
            if e.isUnanswered {
                cg.saveGState()
                box.addClip()
                bar.setFill()
                CGRect(x: r.minX, y: r.minY, width: 3, height: r.height).fill()
                cg.restoreGState()
            }
        } else {
            (e.isAway ? EventStyle.awayFill(e, on: surface) : CalendarColor.color(e.record.color, .fill, on: surface)).setFill()
            box.fill()
            cg.saveGState()
            box.addClip()
            if e.isTentative, !e.isAway {
                surface.color.setStroke()
                let stripes = NSBezierPath()
                stripes.lineWidth = 3.5
                var x = r.minX
                while x < r.maxX + r.height {
                    stripes.move(to: CGPoint(x: x, y: r.minY))
                    stripes.line(to: CGPoint(x: x - r.height, y: r.maxY))
                    x += 10
                }
                stripes.stroke()
            }
            bar.setFill()
            CGRect(x: r.minX, y: r.minY, width: 3, height: r.height).fill()
            cg.restoreGState()
        }
        cg.restoreGState()
        EventStyle.drawText(e, in: r, now: content.now, cg: cg)
    }

    private func rect(_ d: DraftBlock) -> CGRect {
        let hh = CalendarLayout.hourHeight
        return CGRect(x: content.gutter + CGFloat(d.day) * columnWidth + 3, y: d.top / 60 * hh + 1,
                      width: max(columnWidth - 7, 4), height: max(16, (d.bottom - d.top) / 60 * hh - 2))
    }

    /// A block that isn't saved yet: dashed in its calendar's color, with
    /// its title and times, and the accent ring when `ringed`.
    private func drawDraft(_ d: DraftBlock, ringed: Bool, cg: CGContext) {
        let r = rect(d)
        let bar = CalendarColor.color(d.color, .bar, on: content.surface)
        let box = NSBezierPath(roundedRect: r, xRadius: 6, yRadius: 6)
        content.surface.color.setFill()
        box.fill()
        bar.withAlphaComponent(0.12).setFill()
        box.fill()
        let edge = NSBezierPath(roundedRect: r.insetBy(dx: 0.75, dy: 0.75), xRadius: 5.5, yRadius: 5.5)
        edge.lineWidth = 1.5
        edge.setLineDash([4, 3], count: 2, phase: 0)
        bar.withAlphaComponent(0.85).setStroke()
        edge.stroke()
        if ringed { EventStyle.ring(r, radius: 5.5, gap: 3) }
        cg.textMatrix = CGAffineTransform(scaleX: 1, y: -1)
        let width = r.width - 12
        guard width > 14 else { return }
        var top = r.minY + (r.height < 30 ? 1 : 3)
        for (text, font, color) in [(d.title, EventStyle.titleFont, Theme.text), (d.range, EventStyle.timeFont, Theme.text2)] {
            guard top + 12 <= r.maxY + 2 else { break }
            let attributed = NSAttributedString(string: text, attributes: [.font: font, kCTForegroundColorFromContextAttributeName as NSAttributedString.Key: true])
            var line = CTLineCreateWithAttributedString(attributed)
            if CTLineGetTypographicBounds(line, nil, nil, nil) > width {
                let dots = CTLineCreateWithAttributedString(NSAttributedString(string: "…", attributes: [.font: font, kCTForegroundColorFromContextAttributeName as NSAttributedString.Key: true]))
                line = CTLineCreateTruncatedLine(line, width, .end, dots) ?? line
            }
            cg.setFillColor(color.cgColor)
            cg.textPosition = CGPoint(x: r.minX + 7, y: top + ceil(EventStyle.titleFont.ascender))
            CTLineDraw(line, cg)
            top += 14
        }
    }

    /// Tells SwiftUI where the selected event is, for the detail card.
    func reportSelection() {
        let frame = content.placed.first { $0.event.id == content.selectedID }.map { box($0).offsetBy(dx: 0, dy: -self.frame.minY) }
        let shown = frame.flatMap { $0.intersects(bounds) ? $0 : nil }
        guard lastReported != .some(shown) else { return }
        lastReported = .some(shown)
        let report = selectedFrame
        DispatchQueue.main.async { report(shown) }
    }

    private func column(at x: CGFloat) -> Int {
        min(max(Int(floor((x - content.gutter) / columnWidth)), 0), max(content.days - 1, 0))
    }

    /// To the nearest 15 minutes.
    private static func snap(_ minutes: Double) -> Int { Int((minutes / 15).rounded()) * 15 }

    /// The grip a press at `point` takes on an item drawn in `r`: an edge
    /// that is its real start or end resizes it.
    private static func grip(_ r: CGRect, _ point: CGPoint, top: Bool, bottom: Bool) -> Press.Grip {
        if top, point.y - r.minY <= 4 { return .top }
        if bottom, r.maxY - point.y <= 4 { return .bottom }
        return .body
    }

    override func mouseDown(with event: NSEvent) {
        let point = convert(event.locationInWindow, from: nil)
        let doc = CGPoint(x: point.x, y: point.y + frame.minY)
        press = nil
        if drop != nil, let d = content.draft, rect(d).contains(doc) {
            press = Press(item: .draft, grip: Self.grip(rect(d), doc, top: true, bottom: true), point: doc, day: d.day,
                          start: d.start, end: d.end, title: d.title, color: d.color)
            return
        }
        guard let placed = content.placed.last(where: { box($0).contains(doc) }) else {
            if event.clickCount == 2, !event.modifierFlags.contains(.option), let create, point.x > content.gutter {
                let day = Int((point.x - content.gutter) / columnWidth)
                let minute = Int(doc.y / CalendarLayout.hourHeight * 60)
                if day < content.days, (0..<24 * 60).contains(minute) { return create(day, minute) }
            }
            guard drop != nil, point.x > content.gutter, !content.columns.isEmpty else { return super.mouseDown(with: event) }
            let day = column(at: doc.x)
            let focus = event.modifierFlags.contains(.option)
            press = Press(item: nil, grip: .body, point: doc, day: day, start: .now, end: .now, title: focus ? "Focus time" : "New event", color: nil)
            press?.focus = focus
            return
        }
        let hit = placed.event
        if event.clickCount == 2 { return open(hit) }
        select(hit)
        guard drop != nil, let start = hit.start, let end = hit.end, content.columns.indices.contains(placed.day) else { return }
        let day = content.columns[placed.day]
        press = Press(item: .event(hit), grip: Self.grip(box(placed), doc, top: start >= day.start, bottom: end <= day.end), point: doc,
                      day: placed.day, start: start, end: end, title: hit.title, color: hit.record.color)
    }

    /// Dragging empty time draws a new event, or focus time with ⌥; dragging an event or the
    /// editor's draft moves it, across days too, and dragging an edge
    /// resizes it, all in 15-minute steps. ⌥ copies an event.
    override func mouseDragged(with event: NSEvent) {
        guard var press else { return }
        autoscroll(with: event)
        let point = convert(event.locationInWindow, from: nil)
        let doc = CGPoint(x: point.x, y: point.y + frame.minY)
        if !press.moving {
            guard hypot(doc.x - press.point.x, doc.y - press.point.y) >= 4 else { return }
            press.moving = true
            press.refused = press.item.map { !canMove($0) } ?? false
        }
        let hh = Double(CalendarLayout.hourHeight)
        let minute = doc.y / hh * 60, pressed = press.point.y / hh * 60
        let columns = content.columns
        let at = { (day: Int, minute: Int) in CalendarModel.at(columns[day].start, minutesOn: minute) }
        var start = press.start, end = press.end
        switch (press.item, press.grip) {
        case (nil, _):
            let a = Self.snap(pressed), b = Self.snap(minute)
            let top = max(0, min(min(a, b), 1440 - 15)), bottom = min(1440, max(max(a, b), top + 15))
            press.drop = press.focus ? .focus(day: press.day, top: top, bottom: bottom) : .create(day: press.day, top: top, bottom: bottom)
            (start, end) = (at(press.day, top), at(press.day, bottom))
        case (let item?, .body):
            // The start as minutes on the day it is drawn on, moved by the pointer.
            let first = columns.lastIndex { $0.start <= press.start } ?? 0
            var day = first + column(at: doc.x) - press.day
            var top = Self.snap(press.start.timeIntervalSince(columns[first].start) / 60 + minute - pressed)
            while top < 0, day > 0 { day -= 1; top += 1440 }
            while top >= 1440, day < content.days - 1 { day += 1; top -= 1440 }
            day = min(max(day, 0), content.days - 1)
            top = min(max(top, 0), 1440 - 15)
            if case .event = item { press.copy = event.modifierFlags.contains(.option) }
            press.drop = .move(item, day: day, top: top, copy: press.copy)
            start = at(day, top)
            end = start.addingTimeInterval(press.end.timeIntervalSince(press.start))
        case (let item?, .top):
            let top = max(0, min(Self.snap(minute), Self.snap(press.end.timeIntervalSince(columns[press.day].start) / 60) - 15))
            press.drop = .resize(item, top: true, day: press.day, minute: top)
            start = at(press.day, top)
        case (let item?, .bottom):
            let bottom = min(1440, max(Self.snap(minute), Self.snap(press.start.timeIntervalSince(columns[press.day].start) / 60) + 15, 15))
            press.drop = .resize(item, top: false, day: press.day, minute: bottom)
            end = at(press.day, bottom)
        }
        press.preview = DraftBlock(start: start, end: end, title: press.title, days: columns, color: press.color)
        let cursor: NSCursor = press.refused ? .operationNotAllowed : press.copy ? .dragCopy
            : press.item == nil ? .arrow : press.grip == .body ? .closedHand : .resizeUpDown
        cursor.set()
        self.press = press
        needsDisplay = true
    }

    override func mouseUp(with event: NSEvent) {
        guard let press else { return }
        self.press = nil
        guard press.moving else { return }
        NSCursor.arrow.set()
        window?.invalidateCursorRects(for: self)
        needsDisplay = true
        guard var dropped = press.drop else { return }
        if case .move(.event(let e), let day, let top, _) = dropped {
            dropped = .move(.event(e), day: day, top: top, copy: event.modifierFlags.contains(.option))
        }
        // A refused move still goes to the model, which says why.
        drop?(dropped)
    }

    /// Without the press, the rest of the drag and its mouse-up do nothing.
    func cancelDrag() -> Bool {
        guard press?.moving == true else { return false }
        press = nil
        NSCursor.arrow.set()
        window?.invalidateCursorRects(for: self)
        needsDisplay = true
        return true
    }

    /// The middle of the first event in sight, in window coordinates, where
    /// the bench starts its drag.
    var firstEventPoint: CGPoint? {
        content.placed.lazy.map { self.box($0).offsetBy(dx: 0, dy: -self.frame.minY) }
            .first { self.bounds.contains(CGPoint(x: $0.midX, y: $0.midY)) }
            .map { self.convert(CGPoint(x: $0.midX, y: $0.midY), to: nil) }
    }

    /// The resize cursor on the edges of events the user may change.
    override func resetCursorRects() {
        guard drop != nil else { return }
        func edges(_ r: CGRect, top: Bool, bottom: Bool) {
            let r = r.offsetBy(dx: 0, dy: -frame.minY)
            guard r.intersects(bounds) else { return }
            if top { addCursorRect(CGRect(x: r.minX, y: r.minY, width: r.width, height: 4), cursor: .resizeUpDown) }
            if bottom { addCursorRect(CGRect(x: r.minX, y: r.maxY - 4, width: r.width, height: 4), cursor: .resizeUpDown) }
        }
        for p in content.placed where content.writable.contains(p.event.record.calendarId) && content.columns.indices.contains(p.day) {
            guard let start = p.event.start, let end = p.event.end else { continue }
            let day = content.columns[p.day]
            edges(box(p), top: start >= day.start, bottom: end <= day.end)
        }
        if let d = content.draft { edges(rect(d), top: true, bottom: true) }
    }

    // VoiceOver reads every event by its full label and can open it.
    override func isAccessibilityElement() -> Bool { false }
    override func accessibilityRole() -> NSAccessibility.Role? { .group }
    override func accessibilityLabel() -> String? { "Events" }

    override func accessibilityChildren() -> [Any]? {
        if elements == nil {
            elements = content.placed.map { p in
                let event = p.event, rect = box(p)
                let element = EventElement(label: event.accessibilityLabel, identifier: "event-\(event.title)",
                                           place: { [weak self] in
                                               guard let self else { return .zero }
                                               return self.screenRect(rect.offsetBy(dx: 0, dy: -self.frame.minY))
                                           }) { [weak self] in self?.open(event) }
                element.setAccessibilityParent(self)
                return element
            }
        }
        return elements
    }
}

/// One event for VoiceOver; pressing it opens the event. Its frame is
/// worked out from its view when asked, so scrolling and window moves keep it right.
nonisolated final class EventElement: NSAccessibilityElement {
    private let open: @MainActor @Sendable () -> Void
    private let place: @MainActor @Sendable () -> CGRect

    init(label: String, identifier: String, value: String? = nil, place: @escaping @MainActor @Sendable () -> CGRect, open: @escaping @MainActor @Sendable () -> Void) {
        self.open = open
        self.place = place
        super.init()
        setAccessibilityRole(.button)
        setAccessibilityLabel(label)
        setAccessibilityIdentifier(identifier)
        if let value { setAccessibilityValue(value) }
    }

    override func accessibilityFrame() -> NSRect {
        let place = self.place
        return MainActor.assumeIsolated { place() }
    }

    override func accessibilityPerformPress() -> Bool {
        let open = self.open
        MainActor.assumeIsolated { open() }
        return true
    }
}

extension NSView {
    /// A rectangle in this view as screen coordinates, for accessibility.
    func screenRect(_ rect: CGRect) -> CGRect {
        guard let window else { return .zero }
        return window.convertToScreen(convert(rect, to: nil))
    }
}

// MARK: - Measuring

/// `-JoltaCalBench YES` switches weeks 100 times with the J and K keys (50
/// forward, 50 back, 60 ms apart) and writes, per switch, the time from
/// posting the key to the hour grid's draw and to the next display-link
/// frame after it, the stages in between, and the process's memory before
/// calendar mode, with it open, right after the switches and 15 s later,
/// to `calbench.txt` in the data folder. Then it drags the first event in
/// sight 40 steps, 20 of 5 points down and 20 back, one per frame, and lets
/// go where it began, so nothing is saved; each step is timed the same way.
@MainActor
final class CalendarBench: NSObject {
    static let shared: CalendarBench? = UserDefaults.standard.bool(forKey: "JoltaCalBench") ? CalendarBench() : nil

    private var posted: CFTimeInterval?
    private var drawnAt: CFTimeInterval?
    private var toDraw: [Double] = []
    private var toFrame: [Double] = []
    private var link: CADisplayLink?
    private weak var window: NSWindow?
    private var memoryBefore = 0.0
    private var memoryOpen = 0.0
    private var remaining = 0
    /// The drag's points still to come, once the switches are done.
    private var drags: [CGPoint] = []
    private var dragging = false
    private var dragDraw: [Double] = []
    private var dragFrame: [Double] = []
    private var missed = 0

    func drew() {
        if posted != nil, drawnAt == nil { drawnAt = CACurrentMediaTime() }
    }

    private var stages: [String: [Double]] = [:]

    /// Time since the key was posted, at a point of the switch.
    func mark(_ point: String) {
        if let posted, drawnAt == nil { stages["at " + point, default: []].append((CACurrentMediaTime() - posted) * 1000) }
    }

    /// Times one stage of a switch, such as the model's reload or the layer's draw.
    func time<T>(_ stage: String, _ work: () -> T) -> T {
        let t = CACurrentMediaTime()
        let stage = dragging ? "drag: " + stage : stage
        defer { if posted != nil { stages[stage, default: []].append((CACurrentMediaTime() - t) * 1000) } }
        return work()
    }

    /// Call with mail on screen; `open` switches to calendar mode.
    func run(window: NSWindow, open: @escaping () -> Void) {
        self.window = window
        memoryBefore = Self.footprint()
        open()
        Task {
            try? await Task.sleep(for: .seconds(2))
            memoryOpen = Self.footprint()
            remaining = 100
            link = window.displayLink(target: self, selector: #selector(frame))
            link?.add(to: .main, forMode: .common)
            post()
        }
    }

    private func post() {
        guard let window, remaining > 0 else { return startDrag() }
        let forward = remaining > 50
        remaining -= 1
        let key = forward ? "j" : "k"
        let event = NSEvent.keyEvent(with: .keyDown, location: .zero, modifierFlags: [], timestamp: ProcessInfo.processInfo.systemUptime,
                                     windowNumber: window.windowNumber, context: nil, characters: key, charactersIgnoringModifiers: key,
                                     isARepeat: false, keyCode: forward ? 38 : 40)
        drawnAt = nil
        posted = CACurrentMediaTime()
        if let event {
            NSApp.postEvent(event, atStart: false)
            // A key from the window server wakes the run loop; a posted one
            // waits for the next wake-up, half a frame on average.
            CFRunLoopWakeUp(CFRunLoopGetMain())
        }
    }

    private static func canvas(in view: NSView) -> HourCanvas? {
        if let canvas = view as? HourCanvas, canvas.drop != nil { return canvas }
        return view.subviews.lazy.compactMap(canvas(in:)).first
    }

    private func startDrag() {
        guard let window, let canvas = window.contentView.flatMap(Self.canvas(in:)), let start = canvas.firstEventPoint else { return finish() }
        let down = (1...20).map { CGPoint(x: start.x, y: start.y - CGFloat($0) * 5) }
        drags = down + down.reversed().dropFirst() + [start]
        dragged = canvas
        dragging = true
        mouse(.leftMouseDown, at: start)
        nextDrag()
    }

    private func nextDrag() {
        guard !drags.isEmpty else {
            mouse(.leftMouseUp, at: lastDrag)
            dragging = false
            return finish()
        }
        lastDrag = drags.removeFirst()
        drawnAt = nil
        posted = CACurrentMediaTime()
        mouse(.leftMouseDragged, at: lastDrag)
    }

    private var lastDrag = CGPoint.zero
    private weak var dragged: HourCanvas?

    /// Straight to the canvas: a window in the background takes a posted
    /// mouse-down only as the click that brings it forward.
    private func mouse(_ type: NSEvent.EventType, at point: CGPoint) {
        guard let window, let canvas = dragged,
              let event = NSEvent.mouseEvent(with: type, location: point, modifierFlags: [], timestamp: ProcessInfo.processInfo.systemUptime,
                                             windowNumber: window.windowNumber, context: nil, eventNumber: 0, clickCount: 1, pressure: 1) else { return }
        switch type {
        case .leftMouseDown: canvas.mouseDown(with: event)
        case .leftMouseDragged: canvas.mouseDragged(with: event)
        default: canvas.mouseUp(with: event)
        }
    }

    @objc private func frame(_ link: CADisplayLink) {
        guard let posted else { return }
        // A drag step that draws nothing by the next frame is counted as missed.
        if dragging, drawnAt == nil, CACurrentMediaTime() - posted > 0.1 {
            missed += 1
            self.posted = nil
            return nextDrag()
        }
        guard let drawnAt else { return }
        if dragging {
            dragDraw.append((drawnAt - posted) * 1000)
            dragFrame.append((CACurrentMediaTime() - posted) * 1000)
            self.posted = nil
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.016) { self.nextDrag() }
            return
        }
        toDraw.append((drawnAt - posted) * 1000)
        toFrame.append((CACurrentMediaTime() - posted) * 1000)
        self.posted = nil
        // The next key goes in 60 ms later, from an idle app, as a person's
        // next key would; posting at once timed the previous switch's leftovers.
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.06) { self.post() }
    }

    private func finish() {
        link?.invalidate()
        Task {
            try? await Task.sleep(for: .seconds(1))
            let after = Self.footprint()
            try? await Task.sleep(for: .seconds(15))
            let settled = Self.footprint()
            func stats(_ v: [Double]) -> String {
                let s = v.sorted()
                guard !s.isEmpty else { return "no samples" }
                let p = { (q: Double) in s[min(s.count - 1, Int(Double(s.count - 1) * q))] }
                return String(format: "n %d, p50 %.2f ms, p95 %.2f ms, max %.2f ms", s.count, p(0.5), p(0.95), s.last!)
            }
            let stageLines = stages.keys.sorted().map { "\($0): \(stats(stages[$0]!))" }.joined(separator: "\n")
            let report = """
            week switches, \(Date.now.formatted(date: .numeric, time: .standard))
            \(stageLines)
            key to draw: \(stats(toDraw))
            key to frame: \(stats(toFrame))
            drag step to draw: \(stats(dragDraw))
            drag step to frame: \(stats(dragFrame)), \(missed) steps drew nothing
            memory before calendar \(String(format: "%.1f", memoryBefore)) MB, week open \(String(format: "%.1f", memoryOpen)) MB (added \(String(format: "%.1f", memoryOpen - memoryBefore))), after 100 switches \(String(format: "%.1f", after)) MB (added \(String(format: "%.1f", after - memoryBefore))), 15 s later \(String(format: "%.1f", settled)) MB (added \(String(format: "%.1f", settled - memoryBefore)))

            """
            let url = URL(fileURLWithPath: AppModel.dataDirectory + "/calbench.txt")
            if let handle = try? FileHandle(forWritingTo: url) {
                handle.seekToEndOfFile()
                handle.write(Data(report.utf8))
                try? handle.close()
            } else {
                try? report.write(to: url, atomically: true, encoding: .utf8)
            }
        }
    }

    /// The process's physical footprint in MB, as Activity Monitor's Memory column shows it.
    static func footprint() -> Double {
        var info = task_vm_info_data_t()
        var count = mach_msg_type_number_t(MemoryLayout<task_vm_info_data_t>.size / MemoryLayout<integer_t>.size)
        let result = withUnsafeMutablePointer(to: &info) {
            $0.withMemoryRebound(to: integer_t.self, capacity: Int(count)) { task_info(mach_task_self_, task_flavor_t(TASK_VM_INFO), $0, &count) }
        }
        return result == KERN_SUCCESS ? Double(info.phys_footprint) / 1_048_576 : 0
    }
}

