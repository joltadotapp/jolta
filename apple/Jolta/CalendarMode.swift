@preconcurrency import JoltaFFI
import SwiftUI

/// Calendar mode: the calendar sidebar, then the period's header, view and
/// key hints. ⌘2 opens it and ⌘1 goes back to mail.
struct CalendarModeView: View {
    let model: AppModel
    @State private var cardHeight: CGFloat = 0

    var body: some View {
        let cal = model.calendar
        HStack(spacing: 0) {
            CalendarSidebar(model: model)
                .frame(width: 240)
                .background(Color(Theme.side))
                .overlay(alignment: .trailing) { Rectangle().fill(Color(Theme.line2)).frame(width: 1) }
            VStack(spacing: 0) {
                CalendarHeader(cal: cal)
                Group {
                    switch cal.span {
                    case .day, .workWeek, .week: WeekView(cal: cal)
                    case .month: MonthView(cal: cal)
                    case .agenda: AgendaView(cal: cal)
                    }
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                CalendarFooter(span: cal.span)
            }
            .background(Color(Theme.bg))
            .coordinateSpace(.named(CalendarSpace.main))
            .overlayPreferenceValue(SelectedFrame.self) { frame in
                GeometryReader { geo in
                    if let detail = cal.detail, let event = cal.detailEvent {
                        let anchor = frame ?? CGRect(x: geo.size.width - 380, y: 60, width: 0, height: 0)
                        let x = anchor.maxX + 12 + 340 <= geo.size.width ? anchor.maxX + 12 : max(12, anchor.minX - 352)
                        let y = min(max(anchor.minY - 8, 56), max(56, geo.size.height - cardHeight - 12))
                        EventDetailCard(cal: cal, detail: detail, event: event)
                            .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { cardHeight = $0 }
                            .offset(x: x, y: y)
                            .transition(.opacity)
                    }
                }
            }
        }
        .overlay {
            if let message = cal.lastError {
                Text(message).font(.system(size: 12)).foregroundStyle(Color(Theme.text3)).padding().allowsHitTesting(false)
            }
        }
        .overlay(alignment: .bottom) {
            if let toast = model.toast {
                ToastView(toast: toast, undo: { model.undo() })
                    .padding(.bottom, 48)
                    .transition(.move(edge: .bottom).combined(with: .opacity))
            }
        }
        .animation(.snappy(duration: 0.22), value: model.toast)
    }
}

enum CalendarSpace {
    static let main = "calendarMain"
}

/// The selected event's frame in the calendar's main area, for placing the detail card.
struct SelectedFrame: PreferenceKey {
    static let defaultValue: CGRect? = nil
    static func reduce(value: inout CGRect?, nextValue: () -> CGRect?) { value = value ?? nextValue() }
}

extension View {
    /// Reports this view's frame as the selected event's when `selected`.
    func reportsSelection(_ selected: Bool) -> some View {
        background {
            if selected {
                GeometryReader { g in Color.clear.preference(key: SelectedFrame.self, value: g.frame(in: .named(CalendarSpace.main))) }
            }
        }
    }
}

// MARK: - Header and footer

struct CalendarHeader: View {
    let cal: CalendarModel

    private static let monthYear: DateFormatter = {
        let f = DateFormatter()
        f.setLocalizedDateFormatFromTemplate("MMMM yyyy")
        return f
    }()

    private static let fullDay: DateFormatter = {
        let f = DateFormatter()
        f.setLocalizedDateFormatFromTemplate("EEEE d MMMM yyyy")
        return f
    }()

    private var title: (String, String) {
        guard let first = cal.days.first?.start else { return ("", "") }
        switch cal.span {
        case .day:
            return (Self.fullDay.string(from: first), "")
        case .week, .workWeek:
            let week = Calendar(identifier: .iso8601).component(.weekOfYear, from: first)
            return (Self.monthYear.string(from: first), "Week \(week)")
        case .month:
            let middle = cal.days[cal.days.count / 2].start
            return (Self.monthYear.string(from: middle), "")
        case .agenda:
            return (Self.monthYear.string(from: first), "Next \(CalendarSpan.agendaDays) days")
        }
    }

    var body: some View {
        HStack(spacing: 14) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text(title.0).font(.system(size: 15, weight: .semibold)).tracking(-0.15)
                if !title.1.isEmpty {
                    Text(title.1).font(.system(size: 15)).foregroundStyle(Color(Theme.text3))
                }
            }
            .fixedSize()
            .accessibilityElement(children: .ignore)
            .accessibilityLabel([title.0, title.1].filter { !$0.isEmpty }.joined(separator: ", "))
            .accessibilityIdentifier("calendar-title")
            if let next = cal.nextMeeting {
                NextMeetingLine(next: next, now: cal.now)
            }
            Spacer(minLength: 8)
            SpanSwitcher(cal: cal)
        }
        .padding(.leading, 24)
        .padding(.trailing, 20)
        .frame(height: Theme.headerHeight)
        .background(TitleBarArea())
        .overlay(alignment: .bottom) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
    }
}

/// "Design review now [Join Teams]", "Standup in 12 min [Join]".
struct NextMeetingLine: View {
    let next: NextMeetingRecord
    let now: Date

    var body: some View {
        HStack(spacing: 8) {
            Text("\(next.title) \(CalendarModel.countdown(start: next.start, end: next.end, now: now))")
                .font(.system(size: 12))
                .foregroundStyle(Color(Theme.text2))
                .lineLimit(1)
            JoinButton(title: next.meeting.provider.short.isEmpty ? "Join" : "Join \(next.meeting.provider.short)",
                       live: CalendarModel.isLive(start: next.start, end: next.end, now: now)) {
                CalendarModel.join(next.meeting)
            }
        }
    }
}

/// Join: the accent from 5 minutes before the start until the end, an outline otherwise.
struct JoinButton: View {
    var title = "Join"
    let live: Bool
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Text(title)
                .font(.system(size: 12, weight: live ? .semibold : .medium))
                .foregroundStyle(Color(live ? Theme.raise : Theme.text2))
                .padding(.horizontal, 10)
                .frame(height: 22)
                .background {
                    RoundedRectangle(cornerRadius: 7)
                        .fill(live ? Color(Theme.accent) : .clear)
                        .strokeBorder(live ? .clear : Color(Theme.line), lineWidth: 1)
                }
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .fixedSize()
        .help("Join the online meeting")
    }
}

struct SpanSwitcher: View {
    let cal: CalendarModel

    var body: some View {
        HStack(spacing: 0) {
            ForEach(CalendarSpan.switcher) { span in
                let on = cal.span == span
                Button { cal.span = span } label: {
                    HStack(spacing: 6) {
                        Text(span.title)
                        Text(span.key.uppercased()).font(.system(size: 10, weight: .medium, design: .monospaced)).foregroundStyle(Color(Theme.text3))
                    }
                    .font(.system(size: 12))
                    .foregroundStyle(Color(on ? Theme.text : Theme.text2))
                    .padding(.horizontal, 10)
                    .frame(height: 22)
                    .background {
                        if on {
                            RoundedRectangle(cornerRadius: 6).fill(Color(Theme.raise))
                                .shadow(color: Color(Theme.line), radius: 0, x: 0, y: 0)
                                .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color(Theme.line), lineWidth: 0.5))
                        }
                    }
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .accessibilityAddTraits(on ? .isSelected : [])
            }
        }
        .padding(2)
        .background(RoundedRectangle(cornerRadius: 8).fill(Color(Theme.fill)))
        .fixedSize()
    }
}

struct CalendarFooter: View {
    let span: CalendarSpan

    private var zone: String {
        let id = TimeZone.current.identifier
        return (id.split(separator: "/").last.map(String.init) ?? id).replacingOccurrences(of: "_", with: " ")
    }

    var body: some View {
        HStack(spacing: 16) {
            hint(["J", "K"], span == .day ? "day" : span == .month ? "month" : "week")
            hint(["T"], "today")
            hint(["↵"], "open")
            hint(["1–9"], "calendars")
            hint(["D", "X", "W", "M", "A"], "views")
            Spacer()
            Text("Times in \(zone)")
        }
        .font(.system(size: 11.5))
        .foregroundStyle(Color(Theme.text3))
        .padding(.horizontal, 20)
        .frame(height: 34)
        .overlay(alignment: .top) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
    }

    private func hint(_ keys: [String], _ label: String) -> some View {
        HStack(spacing: 5) {
            ForEach(keys, id: \.self) { KeyCap(key: $0) }
            Text(label)
        }
        .fixedSize()
    }
}

// MARK: - Sidebar

struct CalendarSidebar: View {
    let model: AppModel

    var body: some View {
        let cal = model.calendar
        VStack(alignment: .leading, spacing: 0) {
            TitleBarArea().frame(height: 44)
                .overlay(alignment: .topLeading) { ModeSwitcher(model: model).padding(.top, 5).padding(.leading, ModeSwitcher.leading) }
            ScrollView {
                VStack(alignment: .leading, spacing: 2) {
                    MiniMonth(cal: cal)
                    GroupLabel("Calendars")
                    ForEach(cal.groups) { group in
                        HStack {
                            Text(group.title).fontWeight(.medium).foregroundStyle(Color(Theme.text)).lineLimit(1)
                            Spacer(minLength: 6)
                            Text(group.detail).font(.system(size: 11)).foregroundStyle(Color(Theme.text3))
                        }
                        .padding(.horizontal, 8)
                        .padding(.top, 10)
                        .padding(.bottom, 4)
                        ForEach(group.calendars) { CalendarRow(cal: cal, entry: $0) }
                    }
                    if cal.groups.isEmpty {
                        Text("No calendars yet.")
                            .font(.system(size: 12))
                            .foregroundStyle(Color(Theme.text3))
                            .padding(.horizontal, 8)
                    }
                    Button("Calendar Settings…") { model.openSettings(.calendar) }
                        .buttonStyle(.plain)
                        .font(.system(size: 12))
                        .foregroundStyle(Color(Theme.text3))
                        .padding(.horizontal, 8)
                        .padding(.top, 14)
                }
                .padding(.horizontal, 12)
                .padding(.top, 8)
                .padding(.bottom, 12)
            }
            .scrollIndicators(.never)
        }
        .font(.system(size: 13))
    }
}

/// A calendar in the sidebar: its color when shown, an outline when hidden,
/// and its number key. Clicking it shows or hides it, as its number does.
struct CalendarRow: View {
    let cal: CalendarModel
    let entry: CalendarEntry
    @State private var hovering = false

    var body: some View {
        let c = entry.info
        Button { cal.setShown(c, !c.shown) } label: {
            HStack(spacing: 9) {
                RoundedRectangle(cornerRadius: 3)
                    .fill(c.shown ? Color(CalendarColor.color(c.color, .bar)) : .clear)
                    .strokeBorder(c.shown ? .clear : Color(Theme.text3), lineWidth: 1.5)
                    .frame(width: 10, height: 10)
                Text(c.name).lineLimit(1).foregroundStyle(Color(c.shown ? Theme.text2 : Theme.text3))
                if c.syncError != nil {
                    Image(systemName: "exclamationmark.triangle").font(.system(size: 10)).foregroundStyle(Color(Theme.text3))
                        .help(c.syncError ?? "")
                }
                Spacer(minLength: 4)
                if let n = entry.number {
                    Text("\(n)")
                        .font(.system(size: 10, weight: .medium))
                        .foregroundStyle(Color(Theme.text3))
                        .frame(minWidth: 16, minHeight: 16)
                        .overlay(RoundedRectangle(cornerRadius: 4).strokeBorder(Color(Theme.line), lineWidth: 1))
                }
            }
            .padding(.horizontal, 8)
            .frame(height: 26)
            .contentShape(Rectangle())
            .background(RoundedRectangle(cornerRadius: 7).fill(Color(hovering ? Theme.hover : .clear)))
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
        .accessibilityLabel("\(c.name), \(c.shown ? "shown" : "hidden")")
        .accessibilityValue(entry.number.map { "key \($0)" } ?? "")
        .accessibilityIdentifier("calendar-\(c.name)")
        .help(c.shown ? "Hide \(c.name)" : "Show \(c.name)")
    }
}

/// The month around the period, with the period's days marked, today in
/// the accent's text color and a dot under days that have events.
struct MiniMonth: View {
    let cal: CalendarModel

    private static let title: DateFormatter = {
        let f = DateFormatter()
        f.setLocalizedDateFormatFromTemplate("MMMM yyyy")
        return f
    }()

    private static let cell = CGSize(width: 29.5, height: 24)

    var body: some View {
        let days = cal.miniDays
        let period = Set(cal.days.map(\.key))
        let symbols = Calendar.current.veryShortStandaloneWeekdaySymbols
        let first = Calendar.current.firstWeekday - 1
        let todayKey = CalendarModel.dayKey(cal.now)
        VStack(alignment: .leading, spacing: 2) {
            HStack {
                Text(Self.title.string(from: cal.miniMonth)).fontWeight(.semibold)
                Spacer()
                HStack(spacing: 2) {
                    Button { cal.moveMiniMonth(-1) } label: { Image(systemName: "chevron.left") }
                    Button { cal.moveMiniMonth(1) } label: { Image(systemName: "chevron.right") }
                }
                .buttonStyle(.plain)
                .font(.system(size: 10, weight: .semibold))
                .foregroundStyle(Color(Theme.text3))
            }
            .padding(.horizontal, 8)
            .padding(.bottom, 8)
            HStack(spacing: 0) {
                ForEach(0..<7, id: \.self) { i in
                    Text(symbols[(first + i) % 7]).font(.system(size: 10)).foregroundStyle(Color(Theme.text3)).frame(width: Self.cell.width)
                }
            }
            .padding(.bottom, 2)
            ForEach(0..<(days.count / 7), id: \.self) { row in
                let week = days[row * 7..<row * 7 + 7]
                let marked = week.contains { period.contains($0.key) } && cal.span != .month && cal.span != .agenda
                HStack(spacing: 0) {
                    ForEach(week) { day in
                        MiniDay(day: day, today: day.key == todayKey, busy: cal.busyDays.contains(day.key)) { cal.go(to: day.start) }.equatable()
                            .frame(width: Self.cell.width, height: Self.cell.height)
                    }
                }
                .background { if marked { RoundedRectangle(cornerRadius: 6).fill(Color(Theme.fill)) } }
            }
        }
        .padding(.horizontal, 4)
    }
}

/// One day of the mini month; equal inputs skip it when the period moves.
struct MiniDay: View, Equatable {
    let day: CalendarDay
    let today: Bool
    let busy: Bool
    let go: () -> Void

    static func == (a: MiniDay, b: MiniDay) -> Bool { a.day == b.day && a.today == b.today && a.busy == b.busy }

    var body: some View {
        Button(action: go) {
            Text("\(Calendar.current.component(.day, from: day.start))")
                .font(.system(size: 11, weight: today ? .semibold : .regular).monospacedDigit())
                .foregroundStyle(Color(today ? Theme.accentText : day.outside ? Theme.text3 : Theme.text2))
                .opacity(day.outside ? 0.6 : 1)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .overlay(alignment: .bottom) {
                    if busy { Circle().fill(Color(Theme.text3)).frame(width: 4, height: 4).padding(.bottom, 1) }
                }
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}

// MARK: - Hour grid (day, work week, week)

struct WeekView: View {
    let cal: CalendarModel
    @State private var selectedFrame: CGRect?

    var body: some View {
        WeekGrid(cal: cal, source: .period, selectedFrame: { selectedFrame = $0 })
            .overlay(alignment: .topLeading) {
                if let f = selectedFrame {
                    Color.clear.frame(width: f.width, height: f.height).reportsSelection(true).offset(x: f.minX, y: f.minY).allowsHitTesting(false)
                }
            }
    }
}

// MARK: - Month

struct MonthView: View {
    let cal: CalendarModel

    private static let weekday: DateFormatter = {
        let f = DateFormatter()
        f.setLocalizedDateFormatFromTemplate("EEE")
        return f
    }()

    var body: some View {
        let weeks = cal.days.count / 7
        VStack(spacing: 0) {
            HStack(spacing: 0) {
                ForEach(cal.days.prefix(7)) { day in
                    Text(Self.weekday.string(from: day.start)).font(.system(size: 12)).foregroundStyle(Color(Theme.text3))
                        .padding(.horizontal, 10).padding(.vertical, 8)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
            .overlay(alignment: .bottom) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
            GeometryReader { geo in
                let rowH = geo.size.height / CGFloat(max(weeks, 1))
                VStack(spacing: 0) {
                    ForEach(0..<weeks, id: \.self) { w in
                        HStack(spacing: 0) {
                            ForEach(cal.days[w * 7..<w * 7 + 7]) { day in
                                MonthCell(cal: cal, day: day, height: rowH)
                            }
                        }
                        .frame(height: rowH)
                    }
                }
            }
        }
    }
}

struct MonthCell: View {
    let cal: CalendarModel
    let day: CalendarDay
    let height: CGFloat

    var body: some View {
        let list = cal.events.filter { e in
            if let start = e.start, let end = e.end, !e.isLong { return start < day.end && end > day.start }
            let (a, b) = e.dayKeys
            return a <= day.key && b >= day.key
        }
        let fits = max(0, Int((height - 30) / 18))
        let shown = list.count > fits ? max(0, fits - 1) : list.count
        VStack(alignment: .leading, spacing: 2) {
            Text("\(Calendar.current.component(.day, from: day.start))")
                .font(.system(size: 12, weight: day.isToday ? .semibold : .medium).monospacedDigit())
                .foregroundStyle(Color(day.isToday ? Theme.accentText : day.outside ? Theme.text3 : Theme.text2))
                .padding(.horizontal, 6)
                .padding(.bottom, 2)
                .onTapGesture { cal.span = .day; cal.go(to: day.start) }
            ForEach(list.prefix(shown)) { e in
                MonthLine(cal: cal, event: e)
            }
            if list.count > shown {
                Text("\(list.count - shown) more").font(.system(size: 11)).foregroundStyle(Color(Theme.text3)).padding(.horizontal, 6)
            }
            Spacer(minLength: 0)
        }
        .padding(.top, 6)
        .padding(.horizontal, 3)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background(Color(day.isWeekend || day.outside ? CalendarColor.offHours : .clear))
        .overlay(alignment: .leading) { Rectangle().fill(Color(Theme.line2)).frame(width: 1) }
        .overlay(alignment: .bottom) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
        .clipped()
    }
}

struct MonthLine: View {
    let cal: CalendarModel
    let event: CalEvent

    var body: some View {
        let e = event
        let selected = cal.selectedID == e.id
        HStack(spacing: 5) {
            if let start = e.start, !e.isLong {
                Circle().fill(Color(CalendarColor.color(e.record.color, .bar))).frame(width: 6, height: 6)
                Text(CalendarModel.clockTime(start)).font(.system(size: 10.5, weight: .medium, design: .monospaced)).foregroundStyle(Color(Theme.text3))
            }
            Text(e.title).font(.system(size: 11.5)).foregroundStyle(Color(Theme.text)).strikethrough(e.isDeclined || e.isCancelled).lineLimit(1)
        }
        .padding(.horizontal, 5)
        .frame(maxWidth: .infinity, minHeight: 16, alignment: .leading)
        .background { if e.isLong { RoundedRectangle(cornerRadius: 4).fill(Color(CalendarColor.color(e.record.color, .allDay))) } }
        .overlay { if selected { RoundedRectangle(cornerRadius: 5).strokeBorder(Color(Theme.accent), lineWidth: 2).padding(-1) } }
        .opacity(EventStyle.alpha(e, now: cal.now))
        .reportsSelection(selected)
        .contentShape(Rectangle())
        .onTapGesture(count: 2) { cal.openDetail(e) }
        .onTapGesture { cal.select(e) }
        .accessibilityElement()
        .accessibilityLabel(e.accessibilityLabel)
        .accessibilityAddTraits(.isButton)
        .accessibilityAction { cal.openDetail(e) }
    }
}

// MARK: - Agenda

struct AgendaView: View {
    let cal: CalendarModel

    private static let dayTitle: DateFormatter = {
        let f = DateFormatter()
        f.setLocalizedDateFormatFromTemplate("EEEE d MMMM")
        return f
    }()

    var body: some View {
        let byDay: [(CalendarDay, [CalEvent])] = cal.days.compactMap { day in
            let list = cal.events.filter { e in
                if let start = e.start, let end = e.end, !e.isLong { return start < day.end && end > day.start }
                let (a, b) = e.dayKeys
                return a <= day.key && b >= day.key
            }
            return list.isEmpty ? nil : (day, list)
        }
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 0) {
                    ForEach(byDay, id: \.0.id) { day, list in
                        Text((day.isToday ? "Today, " : "") + Self.dayTitle.string(from: day.start))
                            .font(.system(size: 12, weight: .semibold))
                            .foregroundStyle(Color(day.isToday ? Theme.accentText : Theme.text2))
                            .padding(.horizontal, 24)
                            .padding(.top, 18)
                            .padding(.bottom, 6)
                        // A multi-day event shows under each of its days; the id carries the day.
                        ForEach(list, id: \.id) { e in AgendaRow(cal: cal, event: e).id(day.key + "/" + e.id) }
                    }
                    if byDay.isEmpty {
                        Text("Nothing in these \(CalendarSpan.agendaDays) days")
                            .foregroundStyle(Color(Theme.text3))
                            .frame(maxWidth: .infinity)
                            .padding(.top, 80)
                    }
                }
                .padding(.bottom, 20)
            }
            .onChange(of: cal.selectedID) { _, id in
                if let id, let day = byDay.first(where: { $0.1.contains { $0.id == id } })?.0 { proxy.scrollTo(day.key + "/" + id) }
            }
        }
    }
}

struct AgendaRow: View {
    let cal: CalendarModel
    let event: CalEvent

    var body: some View {
        let e = event
        let selected = cal.selectedID == e.id
        HStack(spacing: 12) {
            Text(e.isLong ? "all day" : CalendarLayout.range(e))
                .font(.system(size: 11.5, weight: .medium, design: .monospaced))
                .foregroundStyle(Color(Theme.text3))
                .frame(width: 92, alignment: .trailing)
            RoundedRectangle(cornerRadius: 1.5)
                .fill(e.isFree ? .clear : Color(CalendarColor.color(e.record.color, .bar)))
                .strokeBorder(e.isFree ? Color(CalendarColor.color(e.record.color, .bar)) : .clear, lineWidth: 1)
                .frame(width: 3, height: 22)
            VStack(alignment: .leading, spacing: 1) {
                Text(e.title).foregroundStyle(Color(Theme.text)).strikethrough(e.isDeclined || e.isCancelled).lineLimit(1)
                let sub = [CalendarLayout.subtitle(e), e.isTentative ? "tentative" : "", e.isDeclined ? "declined" : ""].filter { !$0.isEmpty }
                if !sub.isEmpty {
                    Text(sub.joined(separator: " · ")).font(.system(size: 11.5)).foregroundStyle(Color(Theme.text3)).lineLimit(1)
                }
            }
            Spacer(minLength: 8)
            if let start = e.start, let end = e.end, e.record.joinUrl != nil, !e.isPast(cal.now) {
                JoinButton(live: CalendarModel.isLive(start: Int64(start.timeIntervalSince1970), end: Int64(end.timeIntervalSince1970), now: cal.now)) {
                    cal.join(e)
                }
            }
        }
        .padding(.horizontal, 12)
        .frame(height: 40)
        .background(RoundedRectangle(cornerRadius: 8).fill(Color(selected ? Theme.fill : .clear)))
        .overlay(alignment: .leading) {
            if selected { RoundedRectangle(cornerRadius: 1).fill(Color(Theme.accent)).frame(width: 2, height: 18).padding(.leading, 1) }
        }
        .opacity(EventStyle.alpha(e, now: cal.now))
        .reportsSelection(selected)
        .padding(.horizontal, 12)
        .contentShape(Rectangle())
        .onTapGesture(count: 2) { cal.openDetail(e) }
        .onTapGesture { cal.select(e) }
        .accessibilityElement(children: .ignore)
        .accessibilityLabel(e.accessibilityLabel)
        .accessibilityAddTraits(.isButton)
        .accessibilityAction { cal.openDetail(e) }
    }
}
