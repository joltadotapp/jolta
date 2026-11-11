@preconcurrency import JoltaFFI
import SwiftUI

/// One open editor: the core's draft, its problems, and the typed time
/// fields. The core decides what is locked, what is wrong and what saving
/// does; this keeps the fields in step with the draft.
@MainActor
@Observable
final class EventEditorModel {
    let core: Core
    var draft: EventDraft {
        didSet { if draft != oldValue { changed() } }
    }
    private(set) var problems: [DraftProblem] = []
    /// What the typed time fields show until they are read.
    var startText = ""
    var endText = ""
    /// The end as it was when the user began typing a new one.
    @ObservationIgnored private var endBeforeTyping: Date?
    /// Why the last save or delete didn't go through.
    var failure: String?
    /// The Repeat row's sentence: the series' rule as it opened, then the
    /// one the repeat editor made.
    private(set) var repeatText: String?
    /// The repeat editor, while it is open.
    var repeatPicker: RepeatPicker?
    /// The stored occurrence being edited, hidden in the grid while the draft shows.
    let event: CalEvent?
    @ObservationIgnored var onChange: (() -> Void)?
    /// What the draft's calendar can do: which organizer options it has.
    private(set) var caps: CapabilitiesInfo?
    /// The Add video call menu for the draft's calendar; empty where it
    /// can have no call, and the button is left out.
    private(set) var videoChoices: [VideoChoice] = []

    /// "Teams meeting", as the menu names the call to be made.
    func videoLabel(_ provider: MeetingProvider) -> String {
        videoChoices.first { $0.provider == provider }?.label ?? "\(provider.name) meeting"
    }

    /// A pick from Add video call: the server makes Teams and Meet on save;
    /// Zoom is the personal link saved in Settings › Calendar.
    func addVideo(_ provider: MeetingProvider) {
        if provider == .zoom {
            draft.zoom = (try? core.zoomLink()) ?? nil
        } else {
            draft.video = provider
        }
    }
    /// Where a stored event can move, by calendar: why it can't, or what
    /// the move costs. Empty for a new event, whose calendar is picked freely.
    private(set) var moveChoices: [Int64: MoveChoice] = [:]

    /// The calendar the event is shown in: the one picked for a move, else its own.
    var shownCalendar: Int64 { draft.moveTo ?? draft.calendarId }

    /// Picks the calendar: a new event goes there, a stored one moves there.
    func pick(calendar id: Int64) {
        if isNew {
            draft.calendarId = id
        } else {
            draft.moveTo = id == draft.calendarId ? nil : id
        }
    }

    /// Why the event can't go to the calendar `info`, shown greyed in the
    /// picker; nil when it can.
    func refusal(_ info: CalendarInfo) -> String? {
        if isNew { return info.writable ? nil : "Read-only" }
        if info.id == draft.calendarId { return nil }
        return moveChoices[info.id].map { $0.refusal } ?? "Not open in Jolta"
    }

    /// Saving shows Send an update?, so the button reads "Save and send…".
    private(set) var willSend = false
    /// The organizer options are open.
    var showsOptions = false
    /// Quick create's model filled in a field: the header says "Read by AI".
    var readByAI = false
    /// Accept proposal opened it: "Bo Nilsson proposed this time: …" above the fields.
    var proposal: String?
    /// Quick create found no day or time: the focus starts on the start time.
    var focusTime = false
    /// The room picker under the Room field, while it is open.
    var roomsOpen = false

    init(core: Core, draft: EventDraft, event: CalEvent?) {
        self.core = core
        self.draft = draft
        self.event = event
        repeatText = draft.repeat
        if draft.base != nil {
            moveChoices = Dictionary(((try? core.moveChoices(draft: draft)) ?? []).map { ($0.calendarId, $0) }) { a, _ in a }
        }
        refreshTexts()
        check()
    }

    var editing: String? { event?.id }
    var isNew: Bool { draft.base == nil }
    var isReadOnly: Bool { draft.locks.contains(.title) && draft.locks.contains(.showAs) }

    func locked(_ field: DraftField) -> Bool { draft.locks.contains(field) }

    /// Picks who a name from quick create is.
    func resolve(_ name: UnresolvedPerson, as person: PersonRecord) {
        draft.unresolved.removeAll { $0.typed == name.typed }
        guard !draft.people.contains(where: { $0.address.caseInsensitiveCompare(person.address) == .orderedSame }) else { return }
        draft.people.append(DraftPerson(name: person.name, address: person.address, role: .required, response: .needsAction, comment: "", isMe: false))
    }

    func problems(_ field: DraftField?) -> [DraftProblem] { problems.filter { $0.field == field } }

    var canSave: Bool { !problems.contains(where: \.blocking) }

    private func changed() {
        failure = nil
        check()
        onChange?()
    }

    private func check() {
        problems = (try? core.checkDraft(draft: draft)) ?? []
        willSend = (try? core.planSave(draft: draft))?.dialog != nil
        if caps == nil || calendarOfCaps != draft.calendarId {
            caps = try? core.capabilities(calendarId: draft.calendarId)
            videoChoices = (try? core.videoChoices(calendarId: draft.calendarId)) ?? []
            calendarOfCaps = draft.calendarId
        }
    }

    @ObservationIgnored private var calendarOfCaps: Int64?

    // MARK: Times

    /// Start and end, for a timed draft.
    var interval: (start: Date, end: Date)? {
        guard case .timed(let s, let e) = draft.time else { return nil }
        return (Date(timeIntervalSince1970: TimeInterval(s)), Date(timeIntervalSince1970: TimeInterval(e)))
    }

    var isAllDay: Bool { if case .allDay = draft.time { true } else { false } }

    /// The first day: the start's day, or an all-day event's first day.
    var day: Date {
        switch draft.time {
        case .timed(let s, _): Calendar.current.startOfDay(for: Date(timeIntervalSince1970: TimeInterval(s)))
        case .allDay(let first, _): CalendarModel.date(key: first) ?? .now
        }
    }

    /// Minutes the event lasts, or its days.
    var lengthText: String {
        switch draft.time {
        case .timed(let s, let e):
            let m = Int(e - s) / 60
            guard m > 0 else { return "" }
            return m < 60 ? "\(m) min" : m % 60 == 0 ? "\(m / 60) h" : "\(m / 60) h \(m % 60)"
        case .allDay(let first, let last):
            guard let a = CalendarModel.date(key: first), let b = CalendarModel.date(key: last) else { return "" }
            let days = (Calendar.current.dateComponents([.day], from: a, to: b).day ?? 0) + 1
            return days == 1 ? "1 day" : "\(days) days"
        }
    }

    func refreshTexts() {
        endBeforeTyping = nil
        if let (start, end) = interval {
            startText = CalendarModel.clockTime(start)
            endText = CalendarModel.clockTime(end)
        } else {
            startText = ""
            endText = ""
        }
    }

    /// The wall-clock time on `day`, so a day with a clock change keeps it.
    private func at(_ day: Date, minutes: Int) -> Int64 {
        Int64(CalendarModel.at(day, minutes: minutes).timeIntervalSince1970)
    }

    private func minutes(of date: Date) -> Int {
        let c = Calendar.current.dateComponents([.hour, .minute], from: date)
        return (c.hour ?? 0) * 60 + (c.minute ?? 0)
    }

    /// Moves the event to `date`, keeping its times and length.
    func setDay(_ date: Date) {
        switch draft.time {
        case .timed(let s, let e):
            let start = at(date, minutes: minutes(of: Date(timeIntervalSince1970: TimeInterval(s))))
            draft.time = .timed(start: start, end: start + (e - s))
        case .allDay(let first, let last):
            guard let a = CalendarModel.date(key: first), let b = CalendarModel.date(key: last) else { return }
            let days = Calendar.current.dateComponents([.day], from: a, to: b).day ?? 0
            let end = Calendar.current.date(byAdding: .day, value: days, to: date) ?? date
            draft.time = .allDay(firstDay: CalendarModel.dayKey(date), lastDay: CalendarModel.dayKey(end))
        }
    }

    /// Moves the draft to the typed start as it is typed, keeping the
    /// length; the field's text stays as typed until `commit`.
    func followStart() {
        guard let (start, end) = interval, startText != CalendarModel.clockTime(start), let m = core.parseTimeInput(text: startText) else { return }
        let new = at(start, minutes: Int(m))
        let newEnd = new + Int64(end.timeIntervalSince(start))
        draft.time = .timed(start: new, end: newEnd)
        endText = CalendarModel.clockTime(Date(timeIntervalSince1970: TimeInterval(newEnd)))
        endBeforeTyping = nil
    }

    /// The typed end, on the end's own day, so an event over midnight or
    /// several days keeps its last day. An end typed before the start on
    /// the start's day moves to the next day: "22:00 to 1:00".
    func followEnd() {
        guard let (start, end) = interval else { return }
        // The day comes from the end before typing began: a partial "1" on
        // the way to "16:00" must not move the end to tomorrow for good.
        let from = endBeforeTyping ?? end
        if endText == CalendarModel.clockTime(from) {
            if end != from { draft.time = .timed(start: Int64(start.timeIntervalSince1970), end: Int64(from.timeIntervalSince1970)) }
            return
        }
        guard let m = core.parseTimeInput(text: endText) else { return }
        endBeforeTyping = from
        var new = at(from, minutes: Int(m))
        if new <= Int64(start.timeIntervalSince1970), Calendar.current.isDate(start, inSameDayAs: from) {
            new = at(Calendar.current.date(byAdding: .day, value: 1, to: start) ?? start, minutes: Int(m))
        }
        draft.time = .timed(start: Int64(start.timeIntervalSince1970), end: new)
    }

    /// Leaving a time field or saving: what was typed counts, and the field
    /// shows the time it became, or the time it was when it isn't one.
    func commit() {
        followStart()
        followEnd()
        refreshTexts()
    }

    /// Find a time picked a time: the event moves there, with that length.
    func setTimes(start: Int64, end: Int64) {
        draft.time = .timed(start: start, end: end)
        refreshTexts()
    }

    /// The room picker's room, or none; a typed address has no name.
    func setRoom(address: String, name: String) {
        draft.room = address
        draft.roomName = name
    }

    func setLength(minutes: Int) {
        guard let (start, _) = interval else { return }
        let s = Int64(start.timeIntervalSince1970)
        draft.time = .timed(start: s, end: s + Int64(minutes * 60))
        refreshTexts()
    }

    /// ⌥↑ and ⌥↓: 15 minutes later or earlier. The start keeps the length.
    func nudge(start: Bool, by minutes: Int) {
        guard let (s, e) = interval else { return }
        let d = Int64(minutes * 60)
        let (a, b) = (Int64(s.timeIntervalSince1970), Int64(e.timeIntervalSince1970))
        draft.time = start ? .timed(start: a + d, end: b + d) : .timed(start: a, end: b + d)
        refreshTexts()
    }

    // MARK: Repeat

    /// ⌘R or the Repeat row: the repeat editor on the draft as it is now.
    func openRepeat() {
        guard !locked(.repeat), repeatPicker == nil else { return }
        commit()
        do {
            repeatPicker = try RepeatPicker(core: core, draft: draft)
        } catch {
            failure = "\(error)"
        }
    }

    /// Done: the rule the repeat editor made goes into the draft.
    func finishRepeat() {
        guard let picker = repeatPicker, picker.canFinish else { return }
        repeatPicker = nil
        repeatText = picker.summary.text
        draft.rule = picker.rule
    }

    func setAllDay(_ on: Bool, defaultMinutes: Int) {
        if on, let (start, end) = interval {
            let last = end.addingTimeInterval(-1) > start ? end.addingTimeInterval(-1) : start
            draft.time = .allDay(firstDay: CalendarModel.dayKey(start), lastDay: CalendarModel.dayKey(last))
        } else if !on, case .allDay = draft.time {
            let start = at(day, minutes: 9 * 60)
            draft.time = .timed(start: start, end: start + Int64(defaultMinutes * 60))
        }
        refreshTexts()
    }
}

// MARK: - Panel

/// The editor docked on the right of calendar mode, 400 points wide and full
/// height, beside the week, which draws the draft as it changes.
struct EventEditorPanel: View {
    let model: AppModel
    let editor: EventEditorModel
    @FocusState private var focus: Focus?
    @State private var showsZone = false

    enum Focus: Hashable { case title, start, end, people, room, location, description }

    private var cal: CalendarModel { model.calendar }

    /// The title, or the start time when quick create read no day or time.
    private var firstFocus: Focus { editor.focusTime && editor.interval != nil ? .start : .title }

    var body: some View {
        VStack(spacing: 0) {
            header
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    if let proposal = editor.proposal {
                        HStack(alignment: .top, spacing: 9) {
                            Image(systemName: "clock.arrow.2.circlepath").font(.system(size: 11)).foregroundStyle(Color(Theme.accent)).padding(.top, 2)
                            Text(proposal).fixedSize(horizontal: false, vertical: true)
                        }
                        .font(.system(size: 12))
                        .foregroundStyle(Color(Theme.text))
                        .padding(.horizontal, 10)
                        .padding(.vertical, 8)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(RoundedRectangle(cornerRadius: 8).fill(Color(Theme.fill)))
                        .padding(.bottom, 10)
                        .accessibilityIdentifier("editor-proposal")
                    }
                    if let notice = editor.draft.notice {
                        HStack(alignment: .top, spacing: 9) {
                            Image(systemName: "lock").font(.system(size: 11)).foregroundStyle(Color(Theme.text3)).padding(.top, 2)
                            Text(notice).fixedSize(horizontal: false, vertical: true)
                        }
                        .font(.system(size: 12))
                        .foregroundStyle(Color(Theme.text2))
                        .padding(.horizontal, 10)
                        .padding(.vertical, 8)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .background(RoundedRectangle(cornerRadius: 8).fill(Color(Theme.fill)))
                        .padding(.bottom, 10)
                        .accessibilityIdentifier("editor-notice")
                    }
                    title
                    whenSection
                    if editor.locked(.people) ? !editor.draft.people.isEmpty : editor.caps?.invite == true || !editor.draft.unresolved.isEmpty { peopleSection }
                    placeSection
                    descriptionSection
                    calendarSection
                    settingsSection
                    ForEach(editor.problems(nil), id: \.text) { ProblemLine(problem: $0).padding(.top, 8) }
                    if let failure = editor.failure { ProblemLine(problem: DraftProblem(field: nil, text: failure, blocking: true)).padding(.top, 8) }
                }
                .padding(.leading, 16)
                .padding(.trailing, 20)
                .padding(.bottom, 16)
            }
            .scrollIndicators(.never)
            footer
        }
        .frame(width: 400)
        .frame(maxHeight: .infinity)
        .background(Color(Theme.side))
        .overlay(alignment: .leading) { Rectangle().fill(Color(Theme.line2)).frame(width: 1) }
        .font(.system(size: 12.5))
        .foregroundStyle(Color(Theme.text))
        .defaultFocus($focus, firstFocus)
        .onAppear {
            // The panel is in the window only after this pass; focus then,
            // and once more after quick create's field has let go.
            guard !editor.isReadOnly else { return }
            DispatchQueue.main.async { focus = firstFocus }
            if editor.focusTime { DispatchQueue.main.asyncAfter(deadline: .now() + 0.15) { focus = firstFocus } }
        }
        .onChange(of: focus) { old, new in
            if old == .start || old == .end { editor.commit() }
            if new == .room, roomAccount != nil, !editor.locked(.room) { editor.roomsOpen = true }
        }
        .onChange(of: editor.roomsOpen) { _, open in
            // The picker had the keys; the field after the room takes them back.
            if !open { focus = editor.locked(.location) ? nil : .location }
        }
        // Find a time over the editor takes Esc, Return and ⌘Return.
        .disabled(model.findTime != nil)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("event-editor")
    }

    // MARK: Header and footer

    private var header: some View {
        HStack(spacing: 8) {
            Text(editor.isNew ? "New event" : "Event")
                .font(.system(size: 11, weight: .semibold))
                .tracking(0.2)
                .foregroundStyle(Color(Theme.text3))
            if editor.readByAI {
                Label("Read by AI", systemImage: "sparkles")
                    .font(.system(size: 10.5))
                    .foregroundStyle(Color(Theme.text2))
                    .padding(.horizontal, 6)
                    .frame(height: 18)
                    .background(Capsule().fill(Color(Theme.fill)))
                    .help("The AI read the words the parser couldn't. Check the fields before saving.")
                    .accessibilityIdentifier("editor-read-by-ai")
            }
            Spacer()
            Button { cal.closeEditor() } label: {
                HStack(spacing: 6) {
                    KeyHint(key: "Esc")
                    Image(systemName: "xmark").font(.system(size: 10, weight: .semibold))
                }
                .foregroundStyle(Color(Theme.text3))
                .padding(.horizontal, 6)
                .frame(height: 24)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .help("Close (Esc)")
        }
        .padding(.leading, 20)
        .padding(.trailing, 10)
        .frame(height: 40)
    }

    private var footer: some View {
        HStack(spacing: 8) {
            if !editor.isNew && !editor.isReadOnly {
                Button { cal.deleteFromEditor() } label: {
                    // A guest removes someone else's meeting, after declining it.
                    HStack(spacing: 7) { Text(editor.locked(.title) ? "Remove from calendar…" : "Delete"); KeyHint(key: "⌘⌫") }
                        .foregroundStyle(Color(Theme.dangerText))
                        .padding(.horizontal, 6)
                        .frame(height: 28)
                        .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .keyboardShortcut(editor.repeatPicker == nil ? KeyboardShortcut(.delete, modifiers: .command) : nil)
                .accessibilityIdentifier("editor-delete")
            }
            Spacer()
            if editor.isReadOnly {
                EditorButton(title: "Done", key: "⌘↩", primary: true) { cal.closeEditor() }
                    .keyboardShortcut(.return, modifiers: .command)
                    .accessibilityIdentifier("editor-done")
            } else {
                // The repeat popover takes Esc and Return while it is open.
                EditorButton(title: "Cancel", key: "Esc") { cal.closeEditor() }
                    .keyboardShortcut(editor.repeatPicker == nil ? .cancelAction : nil)
                EditorButton(title: editor.willSend ? "Save and send…" : "Save", key: "⌘↩", primary: true) {
                    commitTimes()
                    cal.saveEditor()
                }
                .keyboardShortcut(editor.repeatPicker == nil ? KeyboardShortcut(.return, modifiers: .command) : nil)
                .disabled(!editor.canSave)
                .accessibilityIdentifier("editor-save")
            }
        }
        .padding(.leading, 12)
        .padding(.trailing, 16)
        .frame(height: 54)
        .overlay(alignment: .top) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
    }

    /// A time typed counts when the user saves with ⌘↩ from its field.
    private func commitTimes() {
        editor.commit()
    }

    // MARK: Fields

    private var title: some View {
        Group {
            if editor.locked(.title) {
                HStack {
                    Text(editor.draft.title.isEmpty ? "(no title)" : editor.draft.title).lineLimit(2)
                    Spacer(minLength: 8)
                    LockGlyph()
                }
                .accessibilityIdentifier("editor-title")
            } else {
                TextField("Title", text: binding(\.title))
                    .textFieldStyle(.plain)
                    .focused($focus, equals: .title)
                    .accessibilityIdentifier("editor-title")
            }
        }
        .font(.system(size: 20, weight: .semibold))
        .tracking(-0.3)
        .padding(.vertical, 5)
        .padding(.leading, 30)
        .padding(.bottom, 8)
    }

    private var whenSection: some View {
        EditorSection {
            EditorRow(icon: "clock") {
                if editor.locked(.time) {
                    HStack(spacing: 6) {
                        Text(Self.dayText(editor.day)).foregroundStyle(Color(Theme.text))
                        if editor.interval != nil {
                            Text("\(editor.startText) to \(editor.endText)").font(.system(size: 12, weight: .medium, design: .monospaced))
                        }
                        Text(editor.lengthText).foregroundStyle(Color(Theme.text3))
                        Spacer(minLength: 4)
                        LockGlyph()
                    }
                } else {
                    HStack(spacing: 6) {
                        DayChip(day: editor.day) { editor.setDay($0) }
                        if editor.interval != nil {
                            timeField(timeText(start: true), focus: .start, id: "editor-start", start: true)
                            Text("to").foregroundStyle(Color(Theme.text3))
                            timeField(timeText(start: false), focus: .end, id: "editor-end", start: false)
                            Menu {
                                ForEach([15, 30, 45, 60, 90, 120], id: \.self) { m in
                                    Button(m < 60 ? "\(m) minutes" : m == 60 ? "1 hour" : m == 90 ? "1.5 hours" : "2 hours") { editor.setLength(minutes: m) }
                                }
                            } label: { Text(editor.lengthText).foregroundStyle(Color(Theme.text3)) }
                                .menuStyle(.borderlessButton)
                                .menuIndicator(.hidden)
                                .fixedSize()
                                .help("Length")
                                .accessibilityIdentifier("editor-length")
                        } else {
                            Text(editor.lengthText).foregroundStyle(Color(Theme.text3))
                        }
                    }
                }
            }
            EditorRow(icon: nil) {
                HStack(spacing: 14) {
                    if !editor.locked(.time) {
                        Toggle("All day", isOn: Binding(get: { editor.isAllDay }, set: { editor.setAllDay($0, defaultMinutes: cal.defaultLength) }))
                            .toggleStyle(.checkbox)
                            .foregroundStyle(Color(Theme.text2))
                    }
                    Button { showsZone.toggle() } label: {
                        HStack(spacing: 5) {
                            Image(systemName: "globe").foregroundStyle(Color(Theme.text3))
                            Text(zoneCity).underline(color: Color(Theme.text3).opacity(0.6))
                        }
                        .foregroundStyle(Color(Theme.text2))
                    }
                    .buttonStyle(.plain)
                    .help("The event's time zone")
                    Spacer()
                }
                .font(.system(size: 12))
            }
            if showsZone {
                EditorRow(icon: nil) {
                    Text(editor.draft.zoneText ?? (editor.draft.zone.isEmpty ? "Floating: the same clock time in every zone" : editor.draft.zone))
                        .font(.system(size: 12))
                        .foregroundStyle(Color(Theme.text3))
                }
            }
            ForEach(editor.problems(.time), id: \.text) { ProblemLine(problem: $0).padding(.leading, 30) }
            EditorRow(icon: "repeat") { repeatRow }
            ForEach(editor.problems(.repeat), id: \.text) { ProblemLine(problem: $0).padding(.leading, 30) }
        }
    }

    @ViewBuilder
    private var repeatRow: some View {
        let text = Text(editor.repeatText ?? "Does not repeat").foregroundStyle(Color(editor.repeatText == nil ? Theme.text2 : Theme.text))
        if editor.locked(.repeat) {
            HStack {
                text
                Spacer()
                LockGlyph()
            }
        } else {
            Button { editor.openRepeat() } label: {
                HStack {
                    text
                    Spacer()
                    Image(systemName: "chevron.right").font(.system(size: 9, weight: .semibold)).foregroundStyle(Color(Theme.text3))
                }
                .padding(.horizontal, 6)
                .frame(height: 26)
                .background { if editor.repeatPicker != nil { RoundedRectangle(cornerRadius: 6).fill(Color(Theme.fill)) } }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .padding(.horizontal, -6)
            .keyboardShortcut("r", modifiers: .command)
            .help("Repeat (⌘R)")
            .accessibilityIdentifier("editor-repeat")
            .anchorPreference(key: RepeatRowAnchor.self, value: .bounds) { $0 }
        }
    }

    /// The city of the event's zone, or the Mac's for floating and all-day times ("Brussels").
    private var zoneCity: String {
        let zone = editor.draft.zone.isEmpty ? TimeZone.current.identifier : editor.draft.zone
        return (zone.split(separator: "/").last.map(String.init) ?? zone).replacingOccurrences(of: "_", with: " ")
    }

    private func timeText(start: Bool) -> Binding<String> {
        Binding(get: { start ? editor.startText : editor.endText }, set: { if start { editor.startText = $0 } else { editor.endText = $0 } })
    }

    private func timeField(_ text: Binding<String>, focus field: Focus, id: String, start: Bool) -> some View {
        TextField(start ? "Start" : "End", text: text)
            .textFieldStyle(.plain)
            .font(.system(size: 12, weight: .medium, design: .monospaced))
            .multilineTextAlignment(.center)
            .frame(width: 46)
            .fieldBox(focused: focus == field)
            .focused($focus, equals: field)
            .onSubmit { editor.commit() }
            .onChange(of: text.wrappedValue) { start ? editor.followStart() : editor.followEnd() }
            .onKeyPress(keys: [.upArrow, .downArrow], phases: .down) { press in
                guard press.modifiers.contains(.option) else { return .ignored }
                editor.commit()
                editor.nudge(start: start, by: press.key == .upArrow ? 15 : -15)
                return .handled
            }
            .accessibilityIdentifier(id)
            .help("Type 9, 930, 14:30 or 2pm. ⌥↑ and ⌥↓ move it by 15 minutes.")
    }

    private var peopleSection: some View {
        EditorSection {
            EditorRow(icon: "person.2", top: true) {
                if editor.locked(.people) {
                    VStack(alignment: .leading, spacing: 3) {
                        if let o = editor.draft.organizer, !editor.draft.people.contains(where: { $0.address.caseInsensitiveCompare(o.address) == .orderedSame }) {
                            PersonLine(name: o.name.isEmpty ? o.address : o.name, detail: "Organizer")
                        }
                        ForEach(editor.draft.people, id: \.address) { p in
                            PersonLine(name: p.isMe ? "You" : p.name.isEmpty ? p.address : p.name, detail: Self.personDetail(p, organizer: editor.draft.organizer))
                        }
                    }
                } else {
                    PeopleField(editor: editor, account: cal.calendarInfo(editor.draft.calendarId)?.accountId, focus: $focus)
                }
            }
            // Its suggestions hang over the rows below, Find a time among them.
            .zIndex(1)
            if !editor.draft.responses.isEmpty || emailable != nil || canFindTime {
                EditorRow(icon: nil) {
                    HStack(spacing: 8) {
                        Text(editor.draft.responses).font(.system(size: 12)).foregroundStyle(Color(Theme.text2)).accessibilityIdentifier("editor-responses")
                        Spacer(minLength: 0)
                        if canFindTime {
                            Button { findTime() } label: {
                                HStack(spacing: 5) { Text("Find a time"); KeyHint(key: "⇧⌘F") }
                            }
                            .buttonStyle(.plain)
                            .font(.system(size: 12))
                            .foregroundStyle(Color(Theme.accentText))
                            .keyboardShortcut("f", modifiers: [.command, .shift])
                            .help("See when everyone is free and pick a time (⇧⌘F)")
                            .accessibilityIdentifier("editor-find-time")
                        }
                        if let event = emailable {
                            Button("Email guests") { cal.emailGuests(event) }
                                .buttonStyle(.plain)
                                .font(.system(size: 12))
                                .foregroundStyle(Color(Theme.accentText))
                                .help("Write to the people on this event, for what isn't a calendar update")
                                .accessibilityIdentifier("editor-email-guests")
                        }
                    }
                }
            }
            ForEach(editor.problems(.people), id: \.text) { ProblemLine(problem: $0).padding(.leading, 30) }
            if !editor.locked(.options) && editor.draft.people.contains(where: { !$0.isMe }) {
                EditorRow(icon: nil) {
                    Button { editor.showsOptions.toggle() } label: {
                        HStack(spacing: 5) {
                            Image(systemName: editor.showsOptions ? "chevron.down" : "chevron.right").font(.system(size: 9, weight: .semibold))
                            Text("Organizer options")
                        }
                        .foregroundStyle(Color(Theme.text2))
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .accessibilityIdentifier("editor-options")
                }
                if editor.showsOptions {
                    EditorRow(icon: nil) {
                        OrganizerOptions(editor: editor, caps: editor.caps, google: cal.calendarInfo(editor.draft.calendarId)?.kind == .google)
                    }
                }
            }
        }
        .zIndex(1)
    }

    /// The account whose provider Find a time and the room picker ask: the
    /// one of the event's calendar. None for a calendar on this Mac.
    private var roomAccount: Int64? { cal.calendarInfo(editor.draft.calendarId)?.accountId }

    /// Find a time looks the people up through the calendar's account, for a time the user may change.
    private var canFindTime: Bool { roomAccount != nil && !editor.locked(.time) && editor.interval != nil }

    /// ⇧⌘F: Find a time with this event's people and room; its pick comes
    /// back into this editor with the length chosen there.
    private func findTime() {
        guard let account = roomAccount, let (start, end) = editor.interval else { return }
        editor.commit()
        let people = editor.draft.people.filter { !$0.isMe }.map { PersonRecord(name: $0.name, address: $0.address) }
        model.findTime(for: .editor(account: account, people: people, room: editor.draft.room.trimmingCharacters(in: .whitespaces), editor: ObjectIdentifier(editor)),
                       minutes: max(15, Int64(end.timeIntervalSince(start) / 60)))
    }

    /// The stored event Email guests writes about, when others are on it.
    private var emailable: CalEvent? {
        guard !editor.isNew, let event = editor.event, editor.draft.people.contains(where: { !$0.isMe }) else { return nil }
        return event
    }

    static func personDetail(_ p: DraftPerson, organizer: PersonRecord?) -> String {
        if let o = organizer, o.address.caseInsensitiveCompare(p.address) == .orderedSame { return "Organizer" }
        let answer = p.response == .needsAction ? "no answer" : p.response.word.capitalized
        return p.role == .optional ? "Optional, \(answer)" : answer
    }

    /// A locked row with nothing to say is left out, as is a location that
    /// only names the meeting's video call ("Microsoft Teams Meeting").
    private func shows(_ field: DraftField, _ value: String) -> Bool {
        guard editor.locked(field) else { return true }
        if value.isEmpty { return false }
        guard field == .location, let m = editor.draft.meeting else { return true }
        let v = value.lowercased()
        return !(v == m.joinUrl.lowercased() || (!m.provider.short.isEmpty && v.contains(m.provider.short.lowercased())))
    }

    private var placeSection: some View {
        EditorSection {
            if shows(.room, editor.draft.room) {
                EditorRow(icon: "building.2") { roomField }
            }
            ForEach(editor.problems(.room), id: \.text) { ProblemLine(problem: $0).padding(.leading, 30) }
            if shows(.location, editor.draft.location) {
                EditorRow(icon: "mappin") {
                    plainOrField(.location, text: \.location, prompt: "Add a location", focus: .location, id: "editor-location")
                }
            }
            if let m = editor.draft.meeting {
                EditorRow(icon: "video") {
                    HStack {
                        Text("\(m.provider.name) meeting")
                        Spacer()
                        LockGlyph().help("The video call stays as it is.")
                    }
                }
            } else if let video = editor.draft.video {
                EditorRow(icon: "video") {
                    HStack {
                        Text("\(editor.videoLabel(video)) · link made on save").foregroundStyle(Color(Theme.text2))
                        Spacer()
                        Button { editor.draft.video = nil } label: { Image(systemName: "xmark.circle.fill") }
                            .buttonStyle(.plain)
                            .foregroundStyle(Color(Theme.text3))
                            .help("Remove the video call")
                            .accessibilityLabel("Remove the video call")
                    }
                }
                ForEach(editor.problems(.video), id: \.text) { ProblemLine(problem: $0).padding(.leading, 30) }
            } else if let link = editor.draft.zoom {
                EditorRow(icon: "video") {
                    HStack {
                        Text("Zoom meeting")
                        Text(Self.shortLink(link)).foregroundStyle(Color(Theme.text2)).lineLimit(1).truncationMode(.middle)
                        Spacer()
                        if !editor.locked(.video) {
                            Button { editor.draft.zoom = nil } label: { Image(systemName: "xmark.circle.fill") }
                                .buttonStyle(.plain)
                                .foregroundStyle(Color(Theme.text3))
                                .help("Remove the Zoom link")
                                .accessibilityLabel("Remove the Zoom link")
                        }
                    }
                    .accessibilityIdentifier("editor-zoom")
                }
                ForEach(editor.problems(.video), id: \.text) { ProblemLine(problem: $0).padding(.leading, 30) }
            } else if !editor.locked(.video), !editor.videoChoices.isEmpty {
                EditorRow(icon: "video") {
                    Menu {
                        ForEach(editor.videoChoices, id: \.provider) { choice in
                            Button {
                                editor.addVideo(choice.provider)
                            } label: {
                                Text(choice.label)
                                if let why = choice.refusal { Text(why) }
                            }
                            .disabled(choice.refusal != nil)
                        }
                    } label: {
                        Text("Add video call")
                    }
                    .menuStyle(.button)
                    .buttonStyle(.plain)
                    .fixedSize()
                    .foregroundStyle(Color(Theme.text2))
                    .accessibilityIdentifier("editor-video")
                }
            }
        }
    }

    /// "zoom.us/j/4155550123", without the scheme and the passcode.
    static func shortLink(_ link: String) -> String {
        let bare = link.replacingOccurrences(of: "https://", with: "")
        return String(bare.split(separator: "?").first ?? Substring(bare))
    }

    /// The room picker opens from the Room field, on a click or when the
    /// field gets the focus; it lists rooms and takes a typed address. A
    /// calendar on this Mac has no provider to ask, so its field takes an address.
    @ViewBuilder private var roomField: some View {
        if editor.locked(.room) || roomAccount == nil {
            if editor.locked(.room) {
                plainOrField(.room, text: \.room, prompt: "Add a room", focus: .room, id: "editor-room")
            } else {
                TextField("Add a room", text: Binding(get: { editor.draft.room }, set: { editor.setRoom(address: $0, name: "") }))
                    .textFieldStyle(.plain)
                    .focused($focus, equals: .room)
                    .frame(maxWidth: .infinity)
                    .fieldBox(focused: focus == .room)
                    .accessibilityIdentifier("editor-room")
            }
        } else {
            HStack(spacing: 6) {
                TextField("Add a room", text: .constant(editor.draft.roomName.isEmpty ? editor.draft.room : editor.draft.roomName))
                    .textFieldStyle(.plain)
                    .focused($focus, equals: .room)
                    .help(editor.draft.room)
                    .accessibilityIdentifier("editor-room")
                if !editor.draft.room.isEmpty {
                    Button { editor.setRoom(address: "", name: "") } label: { Image(systemName: "xmark.circle.fill") }
                        .buttonStyle(.plain)
                        .foregroundStyle(Color(Theme.text3))
                        .help("Remove the room")
                        .accessibilityLabel("Remove the room")
                }
            }
            .frame(maxWidth: .infinity)
            .fieldBox(focused: focus == .room || editor.roomsOpen)
            .contentShape(Rectangle())
            .onTapGesture { editor.roomsOpen = true }
            .anchorPreference(key: RoomRowAnchor.self, value: .bounds) { $0 }
        }
    }

    private var descriptionSection: some View {
        EditorSection {
            EditorRow(icon: "text.alignleft", top: true) {
                VStack(alignment: .leading, spacing: 4) {
                    if editor.locked(.description) {
                        if !editor.draft.description.isEmpty {
                            Text(editor.draft.description).foregroundStyle(Color(Theme.text2)).fixedSize(horizontal: false, vertical: true).lineLimit(8)
                        } else {
                            Text("No description").foregroundStyle(Color(Theme.text3))
                        }
                    } else {
                        TextField("Add a description", text: binding(\.description), axis: .vertical)
                            .textFieldStyle(.plain)
                            .lineLimit(3...10)
                            .padding(.vertical, 6)
                            .fieldBox(focused: focus == .description, height: nil)
                            .focused($focus, equals: .description)
                            .accessibilityIdentifier("editor-description")
                    }
                    if let note = editor.draft.descriptionNote {
                        Text(note).font(.system(size: 11.5)).foregroundStyle(Color(Theme.text3))
                    }
                    if editor.draft.meeting != nil && !editor.locked(.description) {
                        Text("The meeting's join details stay below your text.").font(.system(size: 11.5)).foregroundStyle(Color(Theme.text3))
                    }
                }
            }
        }
    }

    private var calendarSection: some View {
        EditorSection {
            EditorRow(icon: "calendar") {
                let current = cal.calendarInfo(editor.shownCalendar)
                HStack(spacing: 6) {
                    if (editor.isNew || !editor.moveChoices.isEmpty) && !editor.locked(.calendar) {
                        Menu {
                            ForEach(cal.groups) { group in
                                Section(group.title) {
                                    ForEach(group.calendars) { entry in
                                        let refusal = editor.refusal(entry.info)
                                        Button { editor.pick(calendar: entry.info.id) } label: {
                                            Text(entry.info.name)
                                            if let refusal { Text(refusal) }
                                        }
                                        .disabled(refusal != nil)
                                    }
                                }
                            }
                        } label: {
                            Text(current?.name ?? "Choose a calendar")
                        }
                        .menuStyle(.borderlessButton)
                        .menuIndicator(.hidden)
                        .frame(width: 170, alignment: .leading)
                        .padding(.leading, 20)
                        .fieldBox(chevrons: true)
                        .overlay(alignment: .leading) { CalendarDot(color: current?.color).padding(.leading, 8).allowsHitTesting(false) }
                        .accessibilityIdentifier("editor-calendar")
                    } else {
                        CalendarDot(color: current?.color)
                        Text(current?.name ?? "")
                    }
                    Spacer()
                    if let group = cal.groups.first(where: { $0.calendars.contains { $0.info.id == editor.shownCalendar } }) {
                        Text(group.detail.isEmpty ? group.title : group.detail)
                            .foregroundStyle(Color(Theme.text3))
                            .font(.system(size: 12))
                            .lineLimit(1)
                            .fixedSize()
                    }
                }
            }
            ForEach(editor.problems(.calendar), id: \.text) { ProblemLine(problem: $0).padding(.leading, 30) }
        }
    }

    private var settingsSection: some View {
        EditorSection {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Text("Your settings").font(.system(size: 11, weight: .semibold)).tracking(0.2).foregroundStyle(Color(Theme.text3))
                Text("Only you see these. Never emailed.").font(.system(size: 11.5)).foregroundStyle(Color(Theme.text3))
            }
            .padding(.top, 2)
            .padding(.bottom, 3)
            SettingLine(label: "Show as", locked: editor.locked(.showAs), value: editor.draft.showAs.word) {
                ForEach(cal.showAsChoices(editor.draft.calendarId), id: \.self) { choice in
                    Button(choice.word) { editor.draft.showAs = choice }
                }
            }
            if let note = editor.draft.showAsNote {
                Text(note).font(.system(size: 11.5)).foregroundStyle(Color(Theme.text3)).padding(.leading, 104)
            }
            SettingLine(label: "Reminder", locked: editor.locked(.reminder), value: editor.draft.reminder.word) {
                ForEach(DraftReminder.menu(isNew: editor.isNew), id: \.self) { choice in
                    Button(choice.word) { editor.draft.reminder = choice }
                }
            }
            SettingLine(label: "Visibility", locked: editor.locked(.visibility), value: editor.draft.visibility.word) {
                ForEach([EventVisibility.default, .private], id: \.self) { choice in
                    Button(choice.word) { editor.draft.visibility = choice }
                }
            }
            ForEach(editor.problems(.showAs) + editor.problems(.reminder) + editor.problems(.visibility), id: \.text) { ProblemLine(problem: $0) }
        }
    }

    @ViewBuilder
    private func plainOrField(_ field: DraftField, text: WritableKeyPath<EventDraft, String>, prompt: String, focus value: Focus, id: String) -> some View {
        if editor.locked(field) {
            HStack {
                Text(editor.draft[keyPath: text].isEmpty ? "None" : editor.draft[keyPath: text])
                    .foregroundStyle(Color(editor.draft[keyPath: text].isEmpty ? Theme.text3 : Theme.text))
                Spacer()
                LockGlyph()
            }
            .accessibilityIdentifier(id)
        } else {
            TextField(prompt, text: binding(text))
                .textFieldStyle(.plain)
                .focused($focus, equals: value)
                .frame(maxWidth: .infinity)
                .fieldBox(focused: focus == value)
                .accessibilityIdentifier(id)
        }
    }

    private func binding(_ key: WritableKeyPath<EventDraft, String>) -> Binding<String> {
        Binding(get: { editor.draft[keyPath: key] }, set: { editor.draft[keyPath: key] = $0 })
    }

    private static let dayFormat: DateFormatter = {
        let f = DateFormatter()
        f.setLocalizedDateFormatFromTemplate("EEE d MMM")
        return f
    }()

    /// "Tue 24 Nov", as the mockup writes it, without the comma some locales add.
    static func dayText(_ date: Date) -> String { dayFormat.string(from: date).replacingOccurrences(of: ",", with: "") }
}

// MARK: - Pieces

/// The Room field's bounds, which the room picker opens under.
struct RoomRowAnchor: PreferenceKey {
    static let defaultValue: Anchor<CGRect>? = nil
    static func reduce(value: inout Anchor<CGRect>?, nextValue: () -> Anchor<CGRect>?) { value = value ?? nextValue() }
}

/// The Repeat row's bounds, which the repeat popover's arrow points at.
struct RepeatRowAnchor: PreferenceKey {
    static let defaultValue: Anchor<CGRect>? = nil
    static func reduce(value: inout Anchor<CGRect>?, nextValue: () -> Anchor<CGRect>?) { value = value ?? nextValue() }
}

/// "Tue 24 Nov", which opens a month to pick another day.
private struct DayChip: View {
    let day: Date
    let pick: (Date) -> Void
    @State private var open = false

    var body: some View {
        Button { open.toggle() } label: {
            Text(EventEditorPanel.dayText(day)).foregroundStyle(Color(Theme.text)).fieldBox(focused: open)
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier("editor-day")
        .popover(isPresented: $open, arrowEdge: .bottom) {
            DatePicker("Day", selection: Binding(get: { day }, set: { pick($0); open = false }), displayedComponents: .date)
                .labelsHidden()
                .datePickerStyle(.graphical)
                .padding(12)
        }
    }
}

/// A group of rows with a hairline above, as in the mockup's `.sec`.
private struct EditorSection<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 5) { content }
            .padding(.vertical, 7)
            .frame(maxWidth: .infinity, alignment: .leading)
            .overlay(alignment: .top) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
    }
}

/// An icon column on the left in place of a label.
private struct EditorRow<Content: View>: View {
    let icon: String?
    var top = false
    @ViewBuilder let content: Content

    var body: some View {
        HStack(alignment: top ? .top : .center, spacing: 14) {
            Group {
                if let icon { Image(systemName: icon) } else { Color.clear }
            }
            .font(.system(size: 12))
            .foregroundStyle(Color(Theme.text3))
            .frame(width: 16, height: 16)
            .padding(.top, top ? 4 : 0)
            content.frame(maxWidth: .infinity, alignment: .leading)
        }
        .frame(minHeight: 26)
    }
}

private struct SettingLine<Choices: View>: View {
    let label: String
    let locked: Bool
    let value: String
    @ViewBuilder let choices: Choices

    var body: some View {
        HStack(spacing: 8) {
            Text(label).foregroundStyle(Color(Theme.text2)).frame(width: 96, alignment: .leading)
            if locked {
                Text(value)
                Spacer()
                LockGlyph()
            } else {
                Menu { choices } label: { Text(value) }
                    .menuStyle(.borderlessButton)
                    .menuIndicator(.hidden)
                    .frame(width: 134, alignment: .leading)
                    .fieldBox(chevrons: true)
                    .accessibilityIdentifier("editor-\(label.lowercased().replacingOccurrences(of: " ", with: "-"))")
                Spacer()
            }
        }
        .frame(minHeight: 26)
    }
}

private struct PersonLine: View {
    let name: String
    let detail: String

    var body: some View {
        HStack(spacing: 6) {
            Text(name).lineLimit(1)
            Text(detail).font(.system(size: 11.5)).foregroundStyle(Color(Theme.text3))
            Spacer(minLength: 4)
            LockGlyph()
        }
        .frame(minHeight: 22)
    }
}

struct ProblemLine: View {
    let problem: DraftProblem

    var body: some View {
        Label(problem.text, systemImage: problem.blocking ? "exclamationmark.circle" : "info.circle")
            .font(.system(size: 11.5))
            .foregroundStyle(problem.blocking ? Color(Theme.dangerText) : Color(Theme.text3))
            .fixedSize(horizontal: false, vertical: true)
            .accessibilityIdentifier("editor-problem")
    }
}

private struct LockGlyph: View {
    var body: some View {
        Image(systemName: "lock.fill").font(.system(size: 9)).foregroundStyle(Color(Theme.text3)).accessibilityLabel("Locked")
    }
}

private struct CalendarDot: View {
    let color: String?

    var body: some View {
        RoundedRectangle(cornerRadius: 3).fill(Color(CalendarColor.color(color, .bar))).frame(width: 10, height: 10)
    }
}

/// A key shown inside a button, outlined in the button's text color.
struct KeyHint: View {
    let key: String

    var body: some View {
        Text(key)
            .font(.system(size: 10, weight: .medium))
            .padding(.horizontal, 4)
            .frame(minWidth: 16, minHeight: 16)
            .overlay(RoundedRectangle(cornerRadius: 4).strokeBorder(.foreground.opacity(0.5), lineWidth: 1))
            .opacity(0.8)
    }
}

struct EditorButton: View {
    let title: String
    let key: String
    var primary = false
    let action: () -> Void
    @Environment(\.isEnabled) private var enabled

    var body: some View {
        Button(action: action) {
            HStack(spacing: 7) {
                Text(title)
                KeyHint(key: key)
            }
            .font(.system(size: 12.5, weight: primary ? .semibold : .medium))
            .foregroundStyle(primary ? Color(Theme.onAccent) : Color(Theme.text))
            .padding(.leading, 12)
            .padding(.trailing, 10)
            .frame(height: 28)
            .background {
                RoundedRectangle(cornerRadius: 7)
                    .fill(primary ? Color(Theme.accent) : Color(Theme.kbd))
                    .strokeBorder(primary ? .clear : Color(Theme.line), lineWidth: 1)
            }
            .opacity(enabled ? 1 : 0.5)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }
}

extension ShowAs {
    var word: String {
        switch self {
        case .free: "Free"
        case .tentative: "Tentative"
        case .busy: "Busy"
        case .outOfOffice: "Out of office"
        case .elsewhere: "Working elsewhere"
        }
    }
}

extension EventVisibility {
    var word: String {
        switch self {
        case .default: "Default"
        case .public: "Public"
        case .private: "Private"
        case .confidential: "Confidential"
        }
    }
}

extension DraftReminder {
    var word: String {
        switch self {
        case .default: "Calendar default"
        case .off: "None"
        case .before(let m) where m == 0: "At the start"
        case .before(let m) where m % 1440 == 0: m == 1440 ? "1 day before" : "\(m / 1440) days before"
        case .before(let m) where m % 60 == 0: m == 60 ? "1 hour before" : "\(m / 60) hours before"
        case .before(let m): "\(m) minutes before"
        }
    }

    static func menu(isNew: Bool) -> [DraftReminder] {
        (isNew ? [.default] : []) + [.off, .before(minutes: 0), .before(minutes: 5), .before(minutes: 10), .before(minutes: 15),
                                     .before(minutes: 30), .before(minutes: 60), .before(minutes: 1440)]
    }
}
