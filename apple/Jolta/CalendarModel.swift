@preconcurrency import JoltaFFI
import AppKit
import Observation

/// Mail or the full calendar, switched with ⌘1 and ⌘2.
enum AppMode: String {
    case mail, calendar
}

/// The period a calendar view shows. Every view is a run of whole days
/// from the Mac's calendar, so a week across a clock change keeps its
/// real midnights.
enum CalendarSpan: String, CaseIterable, Identifiable {
    case day, workWeek, week, month, agenda

    var id: String { rawValue }

    var title: String {
        switch self {
        case .day: "Day"
        case .workWeek: "Work week"
        case .week: "Week"
        case .month: "Month"
        case .agenda: "Agenda"
        }
    }

    var key: String {
        switch self {
        case .day: "d"
        case .workWeek: "x"
        case .week: "w"
        case .month: "m"
        case .agenda: "a"
        }
    }

    /// The order in the view switcher.
    static let switcher: [CalendarSpan] = [.day, .workWeek, .week, .month, .agenda]

    /// Days the agenda lists from its first day.
    static let agendaDays = 14

    /// Whether the view is the hour grid (day, work week, week).
    var isGrid: Bool { self == .day || self == .workWeek || self == .week }
}

/// One day of the period: its midnights from the Mac's calendar and its
/// `YYYY-MM-DD` key, which all-day events are compared against as text.
struct CalendarDay: Identifiable, Equatable {
    let start: Date
    let end: Date
    let key: String
    let isToday: Bool
    let isWeekend: Bool
    /// Month view: the day belongs to the month around it, not the one shown.
    let outside: Bool

    var id: String { key }
}

/// An occurrence as the views draw it, made from the core's record.
struct CalEvent: Identifiable, Equatable {
    /// One occurrence: the event with its start, since a series shares the event id.
    let id: String
    let record: OccurrenceRecord
    /// Timed occurrences only.
    let start: Date?
    let end: Date?

    var title: String { record.title.isEmpty ? "(no title)" : record.title }
    var isTimed: Bool { start != nil }

    /// All-day events, and timed ones of a day or longer, sit in the row above the hours.
    var isLong: Bool {
        guard let start, let end else { return true }
        return end.timeIntervalSince(start) >= 86_400
    }

    var isTentative: Bool { record.status == .tentative || record.showAs == .tentative || record.myResponse == .tentative }
    var isFree: Bool { record.showAs == .free }
    var isDeclined: Bool { record.myResponse == .declined }
    var isCancelled: Bool { record.status == .cancelled }

    func isPast(_ now: Date) -> Bool {
        if let end { return end <= now }
        return false
    }

    /// The first and last day keys it covers (inclusive).
    var dayKeys: (first: String, last: String) {
        switch record.time {
        case .allDay(let first, let last): return (first, last)
        case .timed:
            return (CalendarModel.dayKey(start!), CalendarModel.dayKey(end!.addingTimeInterval(-1)))
        }
    }

    init(_ record: OccurrenceRecord) {
        self.record = record
        switch record.time {
        case .timed(let s, let e):
            start = Date(timeIntervalSince1970: TimeInterval(s))
            end = Date(timeIntervalSince1970: TimeInterval(e))
            id = "\(record.eventId)@\(s)"
        case .allDay(let first, _):
            start = nil
            end = nil
            id = "\(record.eventId)@\(first)"
        }
    }

    /// What VoiceOver reads: "Standup, 9:30 to 9:45, Teams meeting, accepted".
    var accessibilityLabel: String {
        var parts = [title]
        if let start, let end {
            parts.append("\(CalendarModel.spoken(start)) to \(CalendarModel.spoken(end))")
        } else {
            let (first, last) = dayKeys
            parts.append(first == last ? "all day" : "all day, \(CalendarModel.spokenDay(first)) to \(CalendarModel.spokenDay(last))")
        }
        if let provider = record.meetingProvider { parts.append("\(provider.name) meeting") }
        if !record.location.isEmpty, record.meetingProvider == nil || !record.location.hasPrefix("http") { parts.append(record.location) }
        if isCancelled { parts.append("cancelled") }
        if let response = record.myResponse, response != .needsAction { parts.append(response.word) }
        else if isTentative { parts.append("tentative") }
        if isFree { parts.append("free") }
        if record.notSynced { parts.append("not synced yet") }
        return parts.joined(separator: ", ")
    }
}

extension MeetingProvider {
    var name: String {
        switch self {
        case .zoom: "Zoom"
        case .teams: "Microsoft Teams"
        case .meet: "Google Meet"
        case .webex: "Webex"
        case .goTo: "GoTo Meeting"
        case .whereby: "Whereby"
        case .jitsi: "Jitsi"
        case .chime: "Amazon Chime"
        case .slack: "Slack huddle"
        case .faceTime: "FaceTime"
        case .ringCentral: "RingCentral"
        case .zoho: "Zoho Meeting"
        case .other: "Online"
        }
    }

    /// The button's word: "Join Teams".
    var short: String {
        switch self {
        case .teams: "Teams"
        case .meet: "Meet"
        case .other: ""
        default: name
        }
    }
}

extension EventResponse {
    var word: String {
        switch self {
        case .needsAction: "not answered"
        case .accepted: "accepted"
        case .tentative: "maybe"
        case .declined: "declined"
        case .delegated: "delegated"
        }
    }
}

/// A calendar in the sidebar's order, numbered 1 to 9 for its key.
struct CalendarEntry: Identifiable {
    let info: CalendarInfo
    let number: Int?
    var id: Int64 { info.id }
}

/// A group in the sidebar: an account with calendar on, or the calendars of no account.
struct CalendarGroup: Identifiable {
    let id: String
    let title: String
    let detail: String
    let calendars: [CalendarEntry]
}

@MainActor
@Observable
final class CalendarModel {
    let core: Core
    var span: CalendarSpan = CalendarSpan(rawValue: UserDefaults.standard.string(forKey: AppModel.key("CalendarSpan")) ?? "") ?? .week {
        didSet { UserDefaults.standard.set(span.rawValue, forKey: AppModel.key("CalendarSpan")); reload() }
    }
    /// A day inside the period on screen.
    private(set) var anchor: Date = .now
    private(set) var days: [CalendarDay] = []
    private(set) var events: [CalEvent] = []
    /// Today, for the day panel beside mail, whatever period calendar mode shows.
    private(set) var today: CalendarDay?
    private(set) var todayEvents: [CalEvent] = []
    private(set) var todayRevision = 0
    /// Bumped when `events` changes, so the grid redraws once per change.
    private(set) var revision = 0
    var selectedID: String? { didSet { if selectedID != oldValue { updateGrids() } } }
    /// The event whose detail is open.
    private(set) var detail: EventDetail?
    private(set) var detailEvent: CalEvent?
    /// Whether the open detail's event can be deleted from it.
    private(set) var detailDeletable = false
    /// The editor docked on the right, while it is open.
    private(set) var editor: EventEditorModel?
    /// "Discard changes?" before closing an editor with unsaved changes.
    var discard: Confirmation?
    /// Which events of a series a save or delete is for, before any other dialog.
    var scopePrompt: ScopePrompt?
    /// Shows a toast, with the token Undo takes back.
    @ObservationIgnored var toast: ((String, Int64?) -> Void)?
    private(set) var list = CalendarList(accounts: [], otherCalendars: [])
    /// Ticks every 30 seconds: the now line, past events and Join buttons follow it.
    private(set) var now: Date = .now
    private(set) var nextMeeting: NextMeetingRecord?
    /// The mini month's month, which can move apart from the period.
    var miniMonth: Date = .now
    /// Days of the mini month that have events.
    private(set) var busyDays: Set<String> = []
    /// The mini month's six weeks, made when its month changes.
    private(set) var miniDays: [CalendarDay] = []
    /// The "YYYY-MM" `busyDays` was read for; cleared when calendars change.
    @ObservationIgnored private var busyMonth: Substring?
    var lastError: String?
    /// Called after every reload of the calendar list or the occurrences, to reschedule reminders.
    @ObservationIgnored var onChange: (() -> Void)?
    @ObservationIgnored private var ticker: Task<Void, Never>?
    /// The AppKit week views on screen, updated directly on every change.
    @ObservationIgnored private let grids = NSHashTable<CalendarWeekView>.weakObjects()
    /// Occurrences of the periods next to the one on screen, read ahead so
    /// J and K find them; keyed by span and first day, dropped on changes.
    @ObservationIgnored private var ahead: [String: [OccurrenceRecord]] = [:]
    @ObservationIgnored private var aheadTask: Task<Void, Never>?

    static let calendar: Calendar = .current

    init(core: Core) {
        self.core = core
        reloadCalendars()
        refresh()
        ticker = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(30 - Double(Calendar.current.component(.second, from: .now) % 30)))
                guard let self else { return }
                self.tick()
            }
        }
    }

    private func tick() {
        let before = now
        now = .now
        if !Self.calendar.isDate(before, inSameDayAs: now) { refresh() } else { refreshNextMeeting(); updateGrids() }
    }

    // MARK: Period

    nonisolated(unsafe) private static let keyFormat: DateFormatter = {
        let f = DateFormatter()
        f.calendar = Calendar(identifier: .gregorian)
        f.locale = Locale(identifier: "en_US_POSIX")
        f.dateFormat = "yyyy-MM-dd"
        return f
    }()

    /// DateFormatter formats safely from any thread.
    nonisolated static func dayKey(_ date: Date) -> String {
        keyFormat.string(from: date)
    }

    /// The days on screen for the span around `anchor`.
    static func days(for span: CalendarSpan, around anchor: Date, today: Date = .now) -> [CalendarDay] {
        let cal = calendar
        let todayKey = keyFormat.string(from: today)
        let first: Date
        let count: Int
        var month: DateInterval?
        switch span {
        case .day:
            first = cal.startOfDay(for: anchor)
            count = 1
        case .week:
            first = cal.dateInterval(of: .weekOfYear, for: anchor)?.start ?? cal.startOfDay(for: anchor)
            count = 7
        case .workWeek:
            let week = cal.dateInterval(of: .weekOfYear, for: anchor)?.start ?? cal.startOfDay(for: anchor)
            // Monday to Friday, wherever the locale starts its week.
            let offset = (2 - cal.component(.weekday, from: week) + 7) % 7
            first = cal.date(byAdding: .day, value: offset, to: week) ?? week
            count = 5
        case .month:
            let m = cal.dateInterval(of: .month, for: anchor) ?? DateInterval(start: anchor, duration: 0)
            month = m
            first = cal.dateInterval(of: .weekOfYear, for: m.start)?.start ?? m.start
            let lastDay = cal.date(byAdding: .day, value: -1, to: m.end) ?? m.start
            let lastWeekEnd = cal.dateInterval(of: .weekOfYear, for: lastDay)?.end ?? m.end
            count = cal.dateComponents([.day], from: first, to: lastWeekEnd).day ?? 35
        case .agenda:
            first = cal.startOfDay(for: anchor)
            count = CalendarSpan.agendaDays
        }
        return (0..<count).compactMap { i in
            guard let start = cal.date(byAdding: .day, value: i, to: first),
                  let end = cal.date(byAdding: .day, value: 1, to: start) else { return nil }
            let key = keyFormat.string(from: start)
            return CalendarDay(start: start, end: end, key: key, isToday: key == todayKey, isWeekend: cal.isDateInWeekend(start),
                               outside: month.map { !$0.contains(start) } ?? false)
        }
    }

    /// Moves a whole period forward or back.
    func shift(_ direction: Int) {
        let cal = Self.calendar
        let next: Date? = switch span {
        case .day: cal.date(byAdding: .day, value: direction, to: anchor)
        case .week, .workWeek: cal.date(byAdding: .weekOfYear, value: direction, to: anchor)
        case .month: cal.date(byAdding: .month, value: direction, to: anchor)
        case .agenda: cal.date(byAdding: .day, value: direction * CalendarSpan.agendaDays, to: anchor)
        }
        guard let next else { return }
        go(to: next)
    }

    func go(to date: Date) {
        anchor = date
        selectedID = nil
        closeDetail()
        reload()
    }

    func goToday() {
        go(to: .now)
    }

    /// Reads the occurrences for the period on screen: a key press's whole
    /// cost before drawing. One week of a busy calendar is about 0.6 ms in
    /// the core, so this runs on the main thread.
    func reload() {
        if let bench = CalendarBench.shared { return bench.time("model reload (core query, events)") { load() } }
        load()
    }

    private func load() {
        let days = Self.days(for: span, around: anchor, today: now)
        self.days = days
        guard let first = days.first, let last = days.last else { return }
        let key = "\(span.rawValue)/\(first.key)"
        do {
            let records = try ahead[key] ?? core.calendarOccurrences(from: Int64(first.start.timeIntervalSince1970), to: Int64(last.end.timeIntervalSince1970))
            events = records.map(CalEvent.init)
            lastError = nil
        } catch {
            events = []
            lastError = "\(error)"
        }
        if let id = selectedID, !events.contains(where: { $0.id == id }) { selectedID = nil }
        revision += 1
        updateGrids()
        if !Self.calendar.isDate(miniMonth, equalTo: anchor, toGranularity: .month) { miniMonth = anchor }
        if busyMonth != Self.dayKey(miniMonth).prefix(7) { reloadBusyDays() }
        readAhead()
    }

    /// Reads the periods before and after this one off the main thread.
    private func readAhead() {
        aheadTask?.cancel()
        let cal = Self.calendar
        let periods: [(String, Int64, Int64)] = [-1, 1].compactMap { step in
            let date: Date? = switch span {
            case .day: cal.date(byAdding: .day, value: step, to: anchor)
            case .week, .workWeek: cal.date(byAdding: .weekOfYear, value: step, to: anchor)
            case .month: cal.date(byAdding: .month, value: step, to: anchor)
            case .agenda: cal.date(byAdding: .day, value: step * CalendarSpan.agendaDays, to: anchor)
            }
            guard let date, let a = Self.days(for: span, around: date, today: now).first,
                  let b = Self.days(for: span, around: date, today: now).last else { return nil }
            return ("\(span.rawValue)/\(a.key)", Int64(a.start.timeIntervalSince1970), Int64(b.end.timeIntervalSince1970))
        }
        let current = "\(span.rawValue)/\(days.first?.key ?? "")"
        let core = core
        ahead = ahead.filter { $0.key == current }
        aheadTask = Task.detached(priority: .utility) {
            var found: [String: [OccurrenceRecord]] = [:]
            for (key, from, to) in periods {
                if Task.isCancelled { return }
                if let records = try? core.calendarOccurrences(from: from, to: to) { found[key] = records }
            }
            await MainActor.run { [found] in
                guard !Task.isCancelled else { return }
                self.ahead.merge(found) { new, _ in new }
            }
        }
    }

    func attach(_ grid: CalendarWeekView) {
        grids.add(grid)
    }

    func updateGrids() {
        for grid in grids.allObjects { grid.update() }
    }

    /// After calendars or their events changed: the period, the mini
    /// month's dots, today, the next meeting and the reminders.
    func refresh() {
        now = .now
        busyMonth = nil
        ahead = [:]
        load()
        if let open = detailEvent {
            let same = events.first { $0.record.calendarId == open.record.calendarId && $0.record.uid == open.record.uid && $0.record.time == open.record.time }
            if let same { openDetail(same) } else { closeDetail() }
        }
        reloadToday()
        refreshNextMeeting()
        onChange?()
    }

    /// Today's events for the day panel.
    func reloadToday() {
        today = Self.days(for: .day, around: now, today: now).first
        guard let today else { return }
        todayEvents = ((try? core.calendarOccurrences(from: Int64(today.start.timeIntervalSince1970), to: Int64(today.end.timeIntervalSince1970))) ?? [])
            .map(CalEvent.init)
        todayRevision += 1
        updateGrids()
    }

    /// Marks the mini month's days that have something on them, read off
    /// the main thread: six weeks of occurrences took up to 17 ms.
    func reloadBusyDays() {
        busyMonth = Self.dayKey(miniMonth).prefix(7)
        let days = Self.days(for: .month, around: miniMonth)
        miniDays = days
        guard let first = days.first, let last = days.last else { busyDays = []; return }
        let core = core, keys = days.map(\.key), month = busyMonth
        let (from, to) = (Int64(first.start.timeIntervalSince1970), Int64(last.end.timeIntervalSince1970))
        Task.detached(priority: .userInitiated) {
            let records = (try? core.calendarOccurrences(from: from, to: to)) ?? []
            var busy = Set<String>()
            for r in records {
                let (a, b): (String, String) = switch r.time {
                case .allDay(let first, let last): (first, last)
                case .timed(let start, let end): (Self.dayKey(Date(timeIntervalSince1970: TimeInterval(start))),
                                                 Self.dayKey(Date(timeIntervalSince1970: TimeInterval(end - 1))))
                }
                for key in keys where key >= a && key <= b { busy.insert(key) }
            }
            await MainActor.run { [busy] in
                if self.busyMonth == month { self.busyDays = busy }
            }
        }
    }

    func moveMiniMonth(_ direction: Int) {
        if let next = Self.calendar.date(byAdding: .month, value: direction, to: miniMonth) {
            miniMonth = next
            reloadBusyDays()
        }
    }

    // MARK: Calendars

    func reloadCalendars() {
        busyMonth = nil
        do {
            list = try core.calendars()
        } catch {
            lastError = "\(error)"
        }
    }

    /// Accounts with calendar on, then subscriptions, then local calendars,
    /// numbered in that order for the keys 1 to 9.
    var groups: [CalendarGroup] {
        var number = 0
        func entries(_ calendars: [CalendarInfo]) -> [CalendarEntry] {
            calendars.map { c in
                number += 1
                return CalendarEntry(info: c, number: number <= 9 ? number : nil)
            }
        }
        var out: [CalendarGroup] = []
        for a in list.accounts where a.calendarOn && !a.calendars.isEmpty {
            let name = a.account.address
            out.append(CalendarGroup(id: "account-\(a.account.id)", title: name, detail: a.account.provider, calendars: entries(a.calendars)))
        }
        let feeds = list.otherCalendars.filter { $0.kind == .webcal }
        if !feeds.isEmpty { out.append(CalendarGroup(id: "feeds", title: "Subscriptions", detail: "Webcal", calendars: entries(feeds))) }
        let local = list.otherCalendars.filter { $0.kind != .webcal }
        if !local.isEmpty { out.append(CalendarGroup(id: "local", title: "On this Mac only", detail: "", calendars: entries(local))) }
        return out
    }

    /// No calendar of any account is on and there are no other calendars yet.
    var hasNoCalendars: Bool {
        !list.accounts.contains(where: \.calendarOn) && list.otherCalendars.isEmpty
    }

    func color(of calendarID: Int64) -> String? {
        (list.accounts.flatMap(\.calendars) + list.otherCalendars).first { $0.id == calendarID }?.color
    }

    func setShown(_ calendar: CalendarInfo, _ shown: Bool) {
        do {
            try core.setCalendarShown(calendarId: calendar.id, shown: shown)
        } catch {
            lastError = "\(error)"
        }
        reloadCalendars()
        refresh()
    }

    /// `1` to `9` show or hide the calendar with that number.
    func toggleCalendar(number: Int) {
        guard let entry = groups.flatMap(\.calendars).first(where: { $0.number == number }) else { return }
        setShown(entry.info, !entry.info.shown)
    }

    // MARK: Selection and detail

    /// Events in reading order per day: all-day first, then by start.
    private func dayEvents(_ day: CalendarDay) -> [CalEvent] {
        events.filter { e in
            if let start = e.start, let end = e.end, !e.isLong { return start < day.end && end > day.start }
            let (a, b) = e.dayKeys
            return a <= day.key && b >= day.key
        }
    }

    var selected: CalEvent? { selectedID.flatMap { id in events.first { $0.id == id } } }

    /// ↑ and ↓ step through one day's events; ← and → go to the nearest
    /// event in time on the day before or after, past the period's edge to
    /// the next period. Without a selection, the first event at or after now
    /// (or the period's first) is selected.
    func moveSelection(_ key: SelectionMove) {
        guard let current = selected, let dayIndex = days.firstIndex(where: { dayEvents($0).contains(current) }) else {
            if key == .left || key == .right, selectedID == nil { return shift(key == .right ? 1 : -1) }
            selectFirst()
            return
        }
        switch key {
        case .up, .down:
            let list = dayEvents(days[dayIndex])
            guard let i = list.firstIndex(of: current) else { return }
            let j = i + (key == .down ? 1 : -1)
            if list.indices.contains(j) { select(list[j]) }
        case .left, .right:
            let step = key == .right ? 1 : -1
            var d = dayIndex + step
            while days.indices.contains(d) {
                let list = dayEvents(days[d])
                if let nearest = nearest(in: list, to: current) { return select(nearest) }
                d += step
            }
            shift(step)
            let ordered = step > 0 ? days : days.reversed()
            if let day = ordered.first(where: { !dayEvents($0).isEmpty }), let e = nearest(in: dayEvents(day), to: current) {
                select(e)
            }
        }
    }

    enum SelectionMove { case up, down, left, right }

    private func nearest(in list: [CalEvent], to event: CalEvent) -> CalEvent? {
        guard let t = event.start else { return list.first }
        let minutes = { (d: Date) in Self.calendar.component(.hour, from: d) * 60 + Self.calendar.component(.minute, from: d) }
        let target = minutes(t)
        return list.filter(\.isTimed).min { abs(minutes($0.start!) - target) < abs(minutes($1.start!) - target) } ?? list.first
    }

    private func selectFirst() {
        let upcoming = events.filter { $0.isTimed && !$0.isLong }.first { ($0.end ?? .distantPast) > now }
        if let e = upcoming ?? events.first { select(e) }
    }

    func select(_ event: CalEvent) {
        selectedID = event.id
        if detailEvent != nil { openDetail(event) }
    }

    func openSelected() {
        if let event = selected { openDetail(event) } else { selectFirst(); if let event = selected { openDetail(event) } }
    }

    func openDetail(_ event: CalEvent) {
        selectedID = event.id
        if editor != nil { return edit(event) }
        do {
            detail = try core.calendarEvent(eventId: event.record.eventId, time: event.record.time, appCss: Self.descriptionCSS)
            detailEvent = event
            detailDeletable = (try? core.planDelete(eventId: event.record.eventId, time: event.record.time))?.refusal == nil
        } catch {
            lastError = "\(error)"
        }
    }

    func closeDetail() {
        detail = nil
        detailEvent = nil
    }

    /// Settles a conflict the user's way; the queued edit shows at once.
    func applyMine(_ op: Int64) {
        do {
            try core.applyMine(op: op)
            refresh()
        } catch {
            lastError = "\(error)"
        }
    }

    /// Opens one occurrence from elsewhere (a notification, the menu bar):
    /// its day in the current view, selected and open.
    func reveal(eventID: Int64, time: OccurrenceTime?) {
        let day: Date? = switch time {
        case .timed(let start, _): Date(timeIntervalSince1970: TimeInterval(start))
        case .allDay(let first, _): Self.keyFormat.date(from: first)
        case nil: nil
        }
        go(to: day ?? .now)
        let match = events.first { $0.record.eventId == eventID && (time == nil || $0.record.time == time) }
            ?? events.first { $0.record.eventId == eventID }
        if let match { openDetail(match) }
    }

    /// Styles for the description document in the detail.
    static let descriptionCSS = """
    :root { color-scheme: light dark; }
    html, body { margin: 0; padding: 0; background: transparent; color: var(--tx2);
      font: 12px/18px -apple-system, system-ui; -webkit-font-smoothing: antialiased; overflow-wrap: anywhere; }
    a { color: inherit; text-decoration-color: var(--tx3); }
    p { margin: 0 0 8px; }
    """

    // MARK: Editing

    /// Minutes a new event lasts, from Settings › Calendar.
    var defaultLength: Int { Int((try? core.defaultEventLength()) ?? 30) }

    func calendarInfo(_ id: Int64) -> CalendarInfo? {
        (list.accounts.flatMap(\.calendars) + list.otherCalendars).first { $0.id == id }
    }

    /// The Show as choices the calendar holds.
    func showAsChoices(_ calendarID: Int64) -> [ShowAs] {
        let choices = (try? core.capabilities(calendarId: calendarID))?.showAs ?? []
        return choices.isEmpty ? [.busy, .free] : choices
    }

    /// C: a new event at `time`, else the next half hour for the default length.
    func newEvent(at time: OccurrenceTime? = nil) {
        guard confirmLeaving({ self.newEvent(at: time) }) else { return }
        do {
            open(EventEditorModel(core: core, draft: try core.newDraft(calendarId: nil, time: time), event: nil))
        } catch {
            toast?("\(error)", nil)
        }
    }

    /// Double-click on empty time: a new event on that half hour.
    func newEvent(day: CalendarDay, minute: Int) {
        let s = Int64(Self.at(day.start, minutes: minute / 30 * 30).timeIntervalSince1970)
        newEvent(at: .timed(start: s, end: s + Int64(defaultLength * 60)))
    }

    /// E or Edit: the selected event in the editor, locked where it can't change.
    func edit(_ event: CalEvent) {
        guard editor?.editing != event.id else { return }
        guard confirmLeaving({ self.edit(event) }) else { return }
        do {
            selectedID = event.id
            open(EventEditorModel(core: core, draft: try core.draftForEvent(eventId: event.record.eventId, time: event.record.time), event: event))
        } catch {
            toast?("\(error)", nil)
        }
    }

    func editSelected() {
        if let event = selected { edit(event) }
    }

    /// ⌘D: a new event like the selected one, at the same time, without its
    /// people or meeting link: a copy never carries the original's call.
    func duplicateSelected() {
        guard let event = selected, confirmLeaving({ self.duplicateSelected() }) else { return }
        do {
            let source = try core.draftForEvent(eventId: event.record.eventId, time: event.record.time)
            let calendar = calendarInfo(source.calendarId)?.writable == true ? source.calendarId : nil
            var copy = try core.newDraft(calendarId: calendar, time: source.time)
            copy.title = source.title
            copy.location = source.location
            copy.room = source.room
            copy.description = source.description
            copy.showAs = source.showAs
            copy.visibility = source.visibility
            if case .before = source.reminder { copy.reminder = source.reminder }
            open(EventEditorModel(core: core, draft: copy, event: nil))
        } catch {
            toast?("\(error)", nil)
        }
    }

    private func open(_ model: EventEditorModel) {
        closeDetail()
        model.onChange = { [weak self, weak model] in
            guard let self, let model else { return }
            if !self.days.contains(where: { $0.start <= model.day && model.day < $0.end }) { self.anchor = model.day; self.reload() }
            self.updateGrids()
        }
        editor = model
        if !days.contains(where: { $0.start <= model.day && model.day < $0.end }) { anchor = model.day; reload() }
        updateGrids()
    }

    /// Whether the open editor may go: true when nothing would be lost, else
    /// it asks "Discard changes?" and runs `then` on Discard.
    private func confirmLeaving(_ then: @escaping () -> Void) -> Bool {
        guard let editor, hasChanges(editor) else { return true }
        discard = Confirmation(title: "Discard changes?", message: "Your changes to \(editor.draft.title.isEmpty ? "this event" : editor.draft.title) aren't saved.",
                               action: "Discard") { [weak self] in
            self?.dropEditor()
            then()
        }
        return false
    }

    private func hasChanges(_ editor: EventEditorModel) -> Bool {
        if editor.isReadOnly { return false }
        if editor.isNew {
            let d = editor.draft
            return !d.title.isEmpty || !d.description.isEmpty || !d.location.isEmpty || !d.room.isEmpty || d.rule != nil
        }
        return (try? core.planSave(draft: editor.draft))?.unchanged == false
    }

    /// Esc or Cancel: closes, asking first when changes would be lost.
    func closeEditor() {
        guard confirmLeaving({}) else { return }
        dropEditor()
    }

    private func dropEditor() {
        editor = nil
        updateGrids()
    }

    /// ⌘↩: saves through the op queue and closes; Undo takes it back. A
    /// change to one occurrence of a series asks for which events first.
    func saveEditor() {
        guard let editor else { return }
        if editor.isReadOnly { return dropEditor() }
        do {
            let plan = try core.planSave(draft: editor.draft)
            if plan.unchanged { return dropEditor() }
            guard !plan.scopes.isEmpty else { return save(editor, scope: nil) }
            scopePrompt = ScopePrompt(title: "Change a repeating event", line: plan.prompt, choices: plan.scopes) { [weak self] scope in
                self?.save(editor, scope: scope)
            }
        } catch {
            editor.failure = "\(error)"
        }
    }

    private func save(_ editor: EventEditorModel, scope: EditScope?) {
        guard self.editor === editor else { return }
        do {
            let saved = try core.saveEvent(draft: editor.draft, scope: scope)
            let (uid, calendarID, time) = (saved.uid, editor.draft.calendarId, editor.draft.time)
            dropEditor()
            refresh()
            // The event just saved is the selected one, so E and Delete act on
            // it: the occurrence at its new time when it is one of a series.
            let mine = events.filter { $0.record.uid == uid && $0.record.calendarId == calendarID }
            if let event = mine.first(where: { $0.record.time == time }) ?? mine.first { selectedID = event.id }
            toast?(saved.message, saved.token)
        } catch {
            editor.failure = "\(error)"
        }
    }

    /// ⌘⌫ in the editor: deletes the event it edits, wherever the week is
    /// now; for one occurrence of a series, after asking for which events.
    func deleteFromEditor() {
        guard let editor, !editor.isNew else { return }
        do {
            let draft = editor.draft
            let plan = try core.planDeleteDraft(draft: draft)
            if let refusal = plan.refusal { return editor.failure = refusal }
            let remove = { [weak self] (scope: EditScope?) in
                guard let self, self.editor === editor else { return }
                do {
                    self.deleted(try self.core.deleteDraft(draft: draft, scope: scope))
                } catch {
                    editor.failure = "\(error)"
                }
            }
            guard !plan.scopes.isEmpty else { return remove(nil) }
            scopePrompt = deletePrompt(plan) { remove($0) }
        } catch {
            editor.failure = "\(error)"
        }
    }

    /// Delete on the selected event, or the detail's Delete.
    func deleteSelected() {
        if let event = selected ?? detailEvent { delete(event) }
    }

    private func delete(_ event: CalEvent) {
        do {
            let plan = try core.planDelete(eventId: event.record.eventId, time: event.record.time)
            if let refusal = plan.refusal { return toast?(refusal, nil) ?? () }
            guard !plan.scopes.isEmpty else { return deleted(try core.deleteEvent(eventId: event.record.eventId)) }
            let draft = try core.draftForEvent(eventId: event.record.eventId, time: event.record.time)
            scopePrompt = deletePrompt(plan) { [weak self] scope in
                guard let self else { return }
                do {
                    self.deleted(try self.core.deleteDraft(draft: draft, scope: scope))
                } catch {
                    self.toast?("\(error)", nil)
                }
            }
        } catch {
            toast?("\(error)", nil)
        }
    }

    private func deletePrompt(_ plan: DeletePlan, perform: @escaping (EditScope) -> Void) -> ScopePrompt {
        let title = plan.title.isEmpty ? "(no title)" : plan.title
        return ScopePrompt(title: "Delete a repeating event", line: plan.prompt ?? "You're deleting \(title).", choices: plan.scopes, perform: perform)
    }

    private func deleted(_ saved: SavedEdit) {
        if editor != nil { dropEditor() }
        closeDetail()
        selectedID = nil
        refresh()
        toast?(saved.message, saved.token)
    }

    /// Undo of a calendar edit from the toast.
    func undo(token: Int64) -> String {
        do {
            try core.undoCalendar(token: token)
            refresh()
            return "Undone"
        } catch {
            return "\(error)"
        }
    }

    static func date(key: String) -> Date? { keyFormat.date(from: key) }

    /// `minutes` after midnight on the clock of `day`, not that many minutes
    /// after it: on a day with a clock change the two differ by an hour.
    static func at(_ day: Date, minutes: Int) -> Date {
        calendar.date(bySettingHour: minutes / 60 % 24, minute: minutes % 60, second: 0, of: day) ?? day
    }

    // MARK: Meetings

    func refreshNextMeeting() {
        nextMeeting = try? core.nextMeeting(now: Int64(Date.now.timeIntervalSince1970))
    }

    /// From 5 minutes before the start until the end, Join is the accent.
    static func isLive(start: Int64, end: Int64, now: Date) -> Bool {
        let t = Int64(now.timeIntervalSince1970)
        return t >= start - 300 && t < end
    }

    /// Opens the meeting in its app when that app is installed, else the link.
    static func join(_ meeting: MeetingInfo) {
        if let app = meeting.appUrl.flatMap(URL.init(string:)), NSWorkspace.shared.urlForApplication(toOpen: app) != nil {
            NSWorkspace.shared.open(app)
        } else if let url = URL(string: meeting.joinUrl) {
            NSWorkspace.shared.open(url)
        }
    }

    /// Joins an occurrence from the grid, which carries only the link: the
    /// detail says whether there is an app link.
    func join(_ event: CalEvent) {
        if let meeting = (try? core.calendarEvent(eventId: event.record.eventId, time: event.record.time, appCss: ""))?.meeting {
            Self.join(meeting)
        } else if let url = event.record.joinUrl.flatMap(URL.init(string:)) {
            NSWorkspace.shared.open(url)
        }
    }

    // MARK: Text

    private static let spokenTime: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "H:mm"
        return f
    }()

    private static let spokenDayFormat: DateFormatter = {
        let f = DateFormatter()
        f.setLocalizedDateFormatFromTemplate("EEEE d MMMM")
        return f
    }()

    static func spoken(_ date: Date) -> String {
        spokenTime.string(from: date)
    }

    static func spokenDay(_ key: String) -> String {
        keyFormat.date(from: key).map { spokenDayFormat.string(from: $0) } ?? key
    }

    /// `09:30` in this Mac's zone, as in the mail list. Worked out from the
    /// zone's offset: a formatter per event made week switches slow.
    static func clockTime(_ date: Date) -> String {
        let local = Int(date.timeIntervalSince1970) + TimeZone.current.secondsFromGMT(for: date)
        let minutes = ((local / 60) % 1440 + 1440) % 1440
        let digits: [Character] = ["0", "1", "2", "3", "4", "5", "6", "7", "8", "9"]
        let (h, m) = (minutes / 60, minutes % 60)
        return String([digits[h / 10], digits[h % 10], ":", digits[m / 10], digits[m % 10]])
    }

    /// "in 12 min", "now", "at 14:00", "Thu 09:30".
    static func countdown(start: Int64, end: Int64, now: Date) -> String {
        let t = Int64(now.timeIntervalSince1970)
        if t >= start && t < end { return "now" }
        let minutes = Int((start - t + 59) / 60)
        if minutes <= 0 { return "now" }
        if minutes < 60 { return "in \(minutes) min" }
        let date = Date(timeIntervalSince1970: TimeInterval(start))
        if calendar.isDate(date, inSameDayAs: now) { return "at \(clockTime(date))" }
        return Sidebar.wake(start)
    }
}
