import AppKit
import QuartzCore
import SwiftUI

/// Everything the hour grid draws for one period.
struct HourGridContent {
    let placed: [PlacedEvent]
    let weekends: [Bool]
    /// The column of today, for the current-time line.
    let today: Int?
    let todayStart: Date?
    let selectedID: String?
    let now: Date
    let gutter: CGFloat
    /// Bumped with every new set of occurrences.
    let revision: Int

    var days: Int { weekends.count }

    static let empty = HourGridContent(placed: [], weekends: [false], today: nil, todayStart: nil, selectedID: nil, now: .now, gutter: 64, revision: -1)
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
        let w = share - (p.column == 0 ? 3 : 1) - (p.column == p.columns - 1 ? 4 : 1)
        let y = p.top / 60 * CalendarLayout.hourHeight + 1
        let h = max(16, (p.bottom - p.top) / 60 * CalendarLayout.hourHeight - 2)
        return CGRect(x: x, y: y, width: max(w, 4), height: h)
    }

    static func alpha(_ e: CalEvent, now: Date) -> CGFloat {
        if e.isDeclined || e.isCancelled { return 0.45 }
        return e.isPast(now) ? 0.55 : 1
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

    /// Title, times and place, top down, as many lines as the box holds. A
    /// box under 30 points tall gets the title and start on one line; one too
    /// narrow to read gets none.
    static func drawText(_ e: CalEvent, in r: CGRect, cg: CGContext) {
        let width = r.width - 10
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
        let titleLine = line(title, width: width)
        let titleWidth = CTLineGetTypographicBounds(titleLine, nil, nil, nil)
        draw(titleLine, Theme.text)
        if struck {
            Theme.text.setFill()
            CGRect(x: x, y: r.minY + (short ? 1 : 3) + ascent - 4, width: min(titleWidth, width), height: 1).fill()
        }
        guard !short else { return }
        if top + lineHeight <= r.maxY + 2 { draw(line([(CalendarLayout.range(e), timeFont)], width: width), Theme.text2) }
        let sub = CalendarLayout.subtitle(e)
        if !sub.isEmpty, top + lineHeight <= r.maxY + 2 { draw(line([(sub, subFont)], width: width), Theme.text2) }
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
    /// The hour at the top when it first shows.
    var firstHour = 7.5
    var select: ((CalEvent) -> Void)?
    var open: ((CalEvent) -> Void)?
    /// The selected event's frame in this view, or nil when it isn't in sight.
    var selectedFrame: (CGRect?) -> Void = { _ in }

    func makeNSView(context: Context) -> CalendarWeekView {
        let view = CalendarWeekView(source: source, gutter: gutter, firstHour: firstHour)
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

    init(source: Source, gutter: CGFloat, firstHour: Double) {
        self.source = source
        strip = DayStrip(gutter: gutter, showsNames: source == .period, showsEmptyLane: source == .period)
        super.init(frame: .zero)
        hours.firstHour = firstHour
        hours.canvas.gutter = gutter
        addSubview(hours)
        addSubview(strip)
        strip.selectedFrame = { [weak self] in self?.stripFrame = $0; self?.reportSelection() }
        hours.canvas.selectedFrame = { [weak self] in self?.gridFrame = $0; self?.reportSelection() }
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    /// Reads the period, events and selection from the model.
    func update() {
        guard let cal = model else { return }
        CalendarBench.shared?.mark("grid update")
        let days: [CalendarDay], events: [CalEvent], revision: Int, selected: String?
        switch source {
        case .period: (days, events, revision, selected) = (cal.days, cal.events, cal.revision, cal.selectedID)
        case .today: (days, events, revision, selected) = (cal.today.map { [$0] } ?? [], cal.todayEvents, cal.todayRevision, nil)
        }
        guard !days.isEmpty else { return }
        let today = days.firstIndex(where: \.isToday)
        strip.content = DayStripContent(days: days, lanes: CalendarLayout.lanes(events, days: days), selectedID: selected, now: cal.now, revision: revision)
        hours.canvas.content = HourGridContent(placed: CalendarLayout.timed(events, days: days), weekends: days.map(\.isWeekend), today: today,
                                               todayStart: today.map { days[$0].start }, selectedID: selected, now: cal.now,
                                               gutter: hours.canvas.gutter, revision: revision)
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

/// Day names, then all-day and multi-day events in up to three lanes.
final class DayStrip: CanvasView {
    let gutter: CGFloat
    let showsNames: Bool
    let showsEmptyLane: Bool
    var content = DayStripContent.empty {
        didSet {
            guard content.revision != oldValue.revision || content.selectedID != oldValue.selectedID
                    || content.now != oldValue.now || content.days != oldValue.days else { return }
            elements = nil
            needsDisplay = true
            reportSelection()
        }
    }
    var select: (CalEvent) -> Void = { _ in }
    var open: (CalEvent) -> Void = { _ in }
    var selectedFrame: (CGRect?) -> Void = { _ in }
    private var elements: [EventElement]?
    private var lastReported: CGRect??

    private static let namesHeight: CGFloat = 36
    private static let laneHeight: CGFloat = 20
    private static let maxLanes = 3
    private static let weekdayFont = NSFont.systemFont(ofSize: 12)
    private static let dayFont = NSFont.monospacedDigitSystemFont(ofSize: 17, weight: .semibold)
    private static let laneFont = NSFont.systemFont(ofSize: 11.5)
    private static let labelFont = NSFont.monospacedSystemFont(ofSize: 10.5, weight: .medium)

    init(gutter: CGFloat, showsNames: Bool, showsEmptyLane: Bool) {
        self.gutter = gutter
        self.showsNames = showsNames
        self.showsEmptyLane = showsEmptyLane
        super.init(frame: .zero)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    private var laneCount: Int { min(Self.maxLanes, (content.lanes.map(\.lane).max() ?? -1) + 1) }
    private var lanesTop: CGFloat { showsNames ? Self.namesHeight + 1 : 0 }

    var height: CGFloat {
        let lanes = laneCount == 0 && !showsEmptyLane ? 0 : max(1, laneCount)
        let laneArea = lanes == 0 ? 0 : 4 + CGFloat(lanes) * (Self.laneHeight + 3) - 3 + 6 + 1
        return lanesTop + laneArea
    }

    private var columnWidth: CGFloat { (bounds.width - gutter) / CGFloat(max(content.days.count, 1)) }

    private func box(_ item: LaneEvent) -> CGRect {
        CGRect(x: gutter + columnWidth * CGFloat(item.first) + 3, y: lanesTop + 4 + CGFloat(item.lane) * (Self.laneHeight + 3),
               width: columnWidth * CGFloat(item.last - item.first + 1) - 6, height: Self.laneHeight)
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
        cg.setFillColor(Theme.text3.cgColor)
        cg.textPosition = CGPoint(x: gutter - 10 - CTLineGetTypographicBounds(label, nil, nil, nil), y: lanesTop + 15)
        CTLineDraw(label, cg)
        for item in content.lanes where item.lane < Self.maxLanes {
            let e = item.event, r = box(item)
            cg.saveGState()
            let alpha = EventStyle.alpha(e, now: content.now)
            if alpha < 1 { cg.setAlpha(alpha) }
            CalendarColor.color(e.record.color, .allDay).setFill()
            NSBezierPath(roundedRect: r, xRadius: 5, yRadius: 5).fill()
            var title = line(e.title, Self.laneFont)
            let width = r.width - 14
            if CTLineGetTypographicBounds(title, nil, nil, nil) > width {
                title = CTLineCreateTruncatedLine(title, max(0, width), .end, line("…", Self.laneFont)) ?? title
            }
            cg.setFillColor(Theme.text.cgColor)
            cg.textPosition = CGPoint(x: r.minX + 7, y: r.minY + 14)
            CTLineDraw(title, cg)
            if e.isDeclined || e.isCancelled {
                Theme.text.setFill()
                CGRect(x: r.minX + 7, y: r.midY, width: min(CTLineGetTypographicBounds(title, nil, nil, nil), width), height: 1).fill()
            }
            cg.restoreGState()
            if e.id == content.selectedID {
                Theme.accent.setStroke()
                let ring = NSBezierPath(roundedRect: r.insetBy(dx: -1.5, dy: -1.5), xRadius: 6.5, yRadius: 6.5)
                ring.lineWidth = 2
                ring.stroke()
            }
        }
        Theme.line2.setFill()
        CGRect(x: 0, y: bounds.height - 1, width: bounds.width, height: 1).fill()
    }

    func reportSelection() {
        let frame = content.lanes.first { $0.event.id == content.selectedID }.map(box)
        guard lastReported != .some(frame) else { return }
        lastReported = .some(frame)
        let report = selectedFrame
        DispatchQueue.main.async { report(frame) }
    }

    override func mouseDown(with event: NSEvent) {
        let point = convert(event.locationInWindow, from: nil)
        guard let hit = content.lanes.last(where: { $0.lane < Self.maxLanes && box($0).contains(point) })?.event else { return super.mouseDown(with: event) }
        if event.clickCount == 2 { open(hit) } else { select(hit) }
    }

    override func isAccessibilityElement() -> Bool { false }
    override func accessibilityRole() -> NSAccessibility.Role? { .group }
    override func accessibilityLabel() -> String? { "All-day events" }

    override func accessibilityChildren() -> [Any]? {
        if elements == nil {
            elements = content.lanes.filter { $0.lane < Self.maxLanes }.map { item in
                let event = item.event
                let element = EventElement(label: event.accessibilityLabel, identifier: "event-\(event.title)") { [weak self] in self?.open(event) }
                element.setAccessibilityParent(self)
                element.setAccessibilityFrameInParentSpace(box(item))
                return element
            }
        }
        return elements
    }
}

final class HourScrollView: NSScrollView {
    let canvas = HourCanvas()
    private let document = Document()
    var firstHour = 7.5
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
            let y = min(firstHour * CalendarLayout.hourHeight, size.height - contentView.bounds.height)
            contentView.scroll(to: NSPoint(x: 0, y: max(0, y)))
            reflectScrolledClipView(contentView)
        }
        follow()
    }

    @objc private func scrolled() { follow() }

    /// Keeps the canvas over the visible part of the day.
    private func follow() {
        let visible = contentView.bounds
        if canvas.frame != visible {
            canvas.frame = visible
            canvas.needsDisplay = true
            canvas.reportSelection()
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
            guard content.revision != oldValue.revision || content.selectedID != oldValue.selectedID
                    || content.weekends != oldValue.weekends || content.now != oldValue.now || content.today != oldValue.today else { return }
            elements = nil
            needsDisplay = true
            reportSelection()
        }
    }
    var select: (CalEvent) -> Void = { _ in }
    var open: (CalEvent) -> Void = { _ in }
    var selectedFrame: (CGRect?) -> Void = { _ in }
    var gutter = CalendarLayout.gutter
    private var elements: [EventElement]?
    private var lastReported: CGRect??

    private var columnWidth: CGFloat { (bounds.width - content.gutter) / CGFloat(max(content.days, 1)) }

    /// An event's box in the document's coordinates.
    private func box(_ p: PlacedEvent) -> CGRect {
        EventStyle.rect(p, width: bounds.width - content.gutter, days: content.days).offsetBy(dx: content.gutter, dy: 0)
    }

    private static let labelFont = NSFont.monospacedSystemFont(ofSize: 10.5, weight: .medium)
    private static let nowFont = NSFont.monospacedSystemFont(ofSize: 10.5, weight: .semibold)

    override func paint(_ cg: CGContext) {
        CalendarBench.shared?.mark("draw start")
        let paint = {
            cg.translateBy(x: 0, y: -self.frame.minY)
            let visible = self.frame
            self.drawGrid(visible, cg: cg)
            for p in self.content.placed {
                let r = self.box(p)
                guard r.intersects(visible) else { continue }
                self.draw(p.event, in: r, cg: cg)
            }
            self.drawNow(cg: cg)
        }
        if let bench = CalendarBench.shared { bench.time("draw (hour grid)", paint) } else { paint() }
        CalendarBench.shared?.drew()
    }

    private func drawGrid(_ visible: CGRect, cg: CGContext) {
        let hh = CalendarLayout.hourHeight
        let colW = columnWidth, x0 = content.gutter
        let height = 24 * hh
        CalendarColor.offHours.setFill()
        for (i, weekend) in content.weekends.enumerated() {
            let x = x0 + CGFloat(i) * colW
            if weekend {
                CGRect(x: x, y: 0, width: colW, height: height).fill()
            } else {
                CGRect(x: x, y: 0, width: colW, height: CGFloat(CalendarLayout.workStart) * hh).fill()
                let end = CGFloat(CalendarLayout.workEnd) * hh
                CGRect(x: x, y: end, width: colW, height: height - end).fill()
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
        drawLabel(CalendarModel.clockTime(content.now), font: Self.nowFont, color: Theme.accentText, y: y, background: Theme.bg, cg: cg)
    }

    private func draw(_ e: CalEvent, in r: CGRect, cg: CGContext) {
        let alpha = EventStyle.alpha(e, now: content.now)
        cg.saveGState()
        if alpha < 1 { cg.setAlpha(alpha) }
        let box = NSBezierPath(roundedRect: r, xRadius: 6, yRadius: 6)
        let bar = CalendarColor.color(e.record.color, .bar)
        if e.isFree {
            Theme.bg.setFill()
            box.fill()
            bar.withAlphaComponent(0.6).setStroke()
            let edge = NSBezierPath(roundedRect: r.insetBy(dx: 0.5, dy: 0.5), xRadius: 5.5, yRadius: 5.5)
            edge.lineWidth = 1
            edge.stroke()
        } else {
            CalendarColor.color(e.record.color, .fill).setFill()
            box.fill()
            cg.saveGState()
            box.addClip()
            if e.isTentative {
                Theme.bg.setStroke()
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
        EventStyle.drawText(e, in: r, cg: cg)
        cg.restoreGState()
        if e.id == content.selectedID {
            Theme.accent.setStroke()
            let ring = NSBezierPath(roundedRect: r.insetBy(dx: -1.5, dy: -1.5), xRadius: 7.5, yRadius: 7.5)
            ring.lineWidth = 2
            ring.stroke()
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

    private func event(at point: CGPoint) -> CalEvent? {
        let p = CGPoint(x: point.x, y: point.y + frame.minY)
        return content.placed.last { box($0).contains(p) }?.event
    }

    override func mouseDown(with event: NSEvent) {
        let point = convert(event.locationInWindow, from: nil)
        guard let hit = self.event(at: point) else { return super.mouseDown(with: event) }
        if event.clickCount == 2 { open(hit) } else { select(hit) }
    }

    // VoiceOver reads every event by its full label and can open it.
    override func isAccessibilityElement() -> Bool { false }
    override func accessibilityRole() -> NSAccessibility.Role? { .group }
    override func accessibilityLabel() -> String? { "Events" }

    override func accessibilityChildren() -> [Any]? {
        if elements == nil {
            elements = content.placed.map { p in
                let event = p.event
                let element = EventElement(label: event.accessibilityLabel, identifier: "event-\(event.title)") { [weak self] in self?.open(event) }
                element.setAccessibilityParent(self)
                element.setAccessibilityFrameInParentSpace(box(p).offsetBy(dx: 0, dy: -frame.minY))
                return element
            }
        }
        return elements
    }
}

/// One event for VoiceOver; pressing it opens the event.
nonisolated final class EventElement: NSAccessibilityElement {
    private let open: @MainActor @Sendable () -> Void

    init(label: String, identifier: String, open: @escaping @MainActor @Sendable () -> Void) {
        self.open = open
        super.init()
        setAccessibilityRole(.button)
        setAccessibilityLabel(label)
        setAccessibilityIdentifier(identifier)
    }

    override func accessibilityPerformPress() -> Bool {
        let open = self.open
        MainActor.assumeIsolated { open() }
        return true
    }
}

// MARK: - Measuring

/// `-JoltaCalBench YES` switches weeks 100 times with the J and K keys (50
/// forward, 50 back, 60 ms apart) and writes, per switch, the time from
/// posting the key to the hour grid's draw and to the next display-link
/// frame after it, the stages in between, and the process's memory before
/// calendar mode, with it open, right after the switches and 15 s later,
/// to `calbench.txt` in the data folder.
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
        guard let window, remaining > 0 else { return finish() }
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

    @objc private func frame(_ link: CADisplayLink) {
        guard let posted, let drawnAt else { return }
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
