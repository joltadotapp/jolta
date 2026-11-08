@preconcurrency import JoltaFFI
import AppKit
import SwiftUI

/// What Find a time was opened for, and so what picking a time does.
struct FindTimeRequest: Hashable {
    enum Purpose: Hashable {
        /// From the palette on a conversation: a reply proposing the time.
        case reply(thread: Int64)
        /// From Propose new time: the time fills the proposal.
        case propose(message: Int64, organizer: String)
        /// From compose: the user's free times go into the message.
        case availability(account: Int64)
    }

    let purpose: Purpose
    /// The length to start from: the meeting's, for a new time.
    var minutes: Int64 = 30
}

extension AppModel {
    func findTime(for purpose: FindTimeRequest.Purpose, minutes: Int64 = 30) {
        palette = nil
        findTime = FindTimeRequest(purpose: purpose, minutes: minutes)
    }

    /// The palette's Find a time on the open conversation.
    func findTimeForConversation() {
        guard let thread = selectedThreadID else { return notify("Open a conversation first") }
        findTime(for: .reply(thread: thread))
    }

    /// The compose palette's Insert my availability.
    func insertMyAvailability() {
        guard let account = compose?.accountId else { return notify("Choose who the message is from first") }
        findTime(for: .availability(account: account))
    }
}

/// One panel for every Find a time: a row per person (the user first)
/// over the next working days, busy blocks drawn as the calendar draws
/// events, people whose times aren't known hatched, suggested times
/// highlighted, time outside a person's working hours shaded. ←→ or j k
/// move between suggestions, − and + change the length, O turns their
/// working hours on and off, R adds a room as one more row, Return picks,
/// Esc closes.
struct FindTimePanel: View {
    @Bindable var model: AppModel
    let request: FindTimeRequest

    private enum Phase {
        case loading
        case ready(FindTimeInfo)
        case calendarOff(accountId: Int64, address: String, needsConsent: Bool)
        case failed(String)
    }

    @State private var phase = Phase.loading
    @State private var slots: [FindTimeSlot] = []
    @State private var selected = 0
    @State private var minutes: Int64 = 30
    @State private var turningOn = false
    /// Suggest only times inside each known person's working hours.
    @State private var theirHours = true
    @State private var keys = ""
    /// Rooms added with + Room, each a row after the people.
    @State private var rooms: [PersonRow] = []
    @State private var pickingRoom = false
    /// A room's times are on their way; picking waits for them.
    @State private var addingRoom = false
    @FocusState private var focused: Bool

    private static let lengths: [Int64] = [15, 30, 45, 60, 90]
    private static let days: UInt32 = 5
    private let nameWidth: CGFloat = 132
    private let rowHeight: CGFloat = 30

    private var forAvailability: Bool {
        if case .availability = request.purpose { true } else { false }
    }

    private var title: String {
        switch request.purpose {
        case .reply: "Find a time"
        case .propose(_, let organizer): organizer.isEmpty ? "Find a new time" : "Find a new time for \(organizer)"
        case .availability: "Insert my availability"
        }
    }

    private var pickVerb: String {
        switch request.purpose {
        case .reply: "Reply with this time"
        case .propose: "Use this time"
        case .availability: "Insert these times"
        }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            header
            Rectangle().fill(Color(Theme.line2)).frame(height: 1)
            content
                .padding(.horizontal, 16)
                .padding(.vertical, 14)
            Rectangle().fill(Color(Theme.line2)).frame(height: 1)
            footer
        }
        .frame(width: 820)
        .background(RoundedRectangle(cornerRadius: 12).fill(Color(Theme.raise)))
        .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(Color(Theme.line), lineWidth: 0.5))
        .shadow(color: .black.opacity(0.3), radius: 24, y: 10)
        .background(keyField)
        .overlay(alignment: .topTrailing) {
            if pickingRoom, case .ready(let info) = phase {
                RoomPicker(
                    core: model.core,
                    accountId: info.accountId,
                    slot: slots.indices.contains(selected) ? slots[selected] : nil,
                    fallbackStart: info.days.first?.start ?? Int64(Date().timeIntervalSince1970),
                    people: info.people.count,
                    taken: Set(rooms.map { $0.address.lowercased() }),
                    add: { address in addRoom(address, info: info) },
                    close: closeRoomPicker
                )
                .padding(.top, 40)
                .padding(.trailing, 12)
            }
        }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("find-time")
        .task {
            minutes = request.minutes > 0 ? request.minutes : 30
            await load()
        }
        .onChange(of: minutes) { suggest() }
        .onChange(of: theirHours) { suggest() }
        .onChange(of: rooms) { suggest() }
    }

    /// Takes the keyboard from the list and the message being written.
    private var keyField: some View {
        TextField("", text: $keys)
            .focused($focused)
            .opacity(0)
            .frame(width: 1, height: 1)
            // The palette that opened the panel gives its field's focus back as it closes.
            .onAppear { DispatchQueue.main.async { focused = true } }
            .onChange(of: keys) { if !keys.isEmpty { keys = "" } }
            .onKeyPress(.escape) { close(); return .handled }
            .onKeyPress(keys: [KeyEquivalent("r")]) { _ in
                guard canAddRoom else { return .ignored }
                pickingRoom = true
                return .handled
            }
            .onKeyPress(.return) { pick(); return .handled }
            .onKeyPress(keys: [.rightArrow, .downArrow, KeyEquivalent("j")]) { _ in move(1); return .handled }
            .onKeyPress(keys: [.leftArrow, .upArrow, KeyEquivalent("k")]) { _ in move(-1); return .handled }
            .onKeyPress(keys: [KeyEquivalent("-"), KeyEquivalent("_")]) { _ in length(-1); return .handled }
            .onKeyPress(keys: [KeyEquivalent("+"), KeyEquivalent("=")]) { _ in length(1); return .handled }
            .onKeyPress(keys: [KeyEquivalent("o")]) { _ in
                guard showsHoursToggle else { return .ignored }
                theirHours.toggle()
                return .handled
            }
            .accessibilityIdentifier("find-time-keys")
    }

    /// Rooms go with a meeting, not with the user's free times.
    private var canAddRoom: Bool {
        guard !forAvailability, case .ready = phase else { return false }
        return true
    }

    /// The panel's answer with the added rooms after the people.
    private func withRooms(_ info: FindTimeInfo) -> FindTimeInfo {
        var all = info
        all.people += rooms
        return all
    }

    /// Looks the room up over the shown days and adds it as a row.
    private func addRoom(_ address: String, info: FindTimeInfo) {
        closeRoomPicker()
        guard !rooms.contains(where: { $0.address.caseInsensitiveCompare(address) == .orderedSame }),
              let first = info.days.first, let last = info.days.last else { return }
        addingRoom = true
        Task {
            do {
                rooms += try await model.core.roomAvailability(accountId: info.accountId, addresses: [address], from: first.start, to: last.end)
            } catch {
                model.notify("\(error)")
            }
            addingRoom = false
        }
    }

    private func closeRoomPicker() {
        pickingRoom = false
        DispatchQueue.main.async { focused = true }
    }

    private var header: some View {
        HStack(spacing: 10) {
            Image(systemName: "calendar.badge.clock").foregroundStyle(Color(Theme.text3))
            Text(title).font(.system(size: 13, weight: .semibold))
            Spacer()
            if canAddRoom {
                Button { pickingRoom = true } label: {
                    HStack(spacing: 5) {
                        Image(systemName: "plus").font(.system(size: 10, weight: .semibold))
                        Text("Room")
                        KeyCap(key: "R")
                    }
                    .font(.system(size: 12))
                    .foregroundStyle(Color(Theme.text2))
                    .padding(.leading, 8)
                    .padding(.trailing, 4)
                    .frame(height: 22)
                    .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color(Theme.line), lineWidth: 1))
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help("Add a room as a row (R)")
                .accessibilityIdentifier("find-time-add-room")
            }
            if showsHoursToggle {
                HStack(spacing: 6) {
                    Toggle("Their working hours", isOn: $theirHours)
                        .toggleStyle(.switch)
                        .controlSize(.mini)
                        .labelsHidden()
                        .accessibilityIdentifier("find-time-their-hours")
                    Text("Their working hours").font(.system(size: 12)).foregroundStyle(Color(Theme.text2))
                    KeyCap(key: "O")
                }
                .help("Suggest only times inside everyone's working hours (O)")
                .padding(.trailing, 4)
            }
            HStack(spacing: 2) {
                Button { length(-1) } label: { Image(systemName: "minus").frame(width: 22, height: 20) }
                Text(lengthText(minutes))
                    .font(.system(size: 12, weight: .medium).monospacedDigit())
                    .frame(minWidth: 64)
                Button { length(1) } label: { Image(systemName: "plus").frame(width: 22, height: 20) }
            }
            .buttonStyle(.plain)
            .foregroundStyle(Color(Theme.text2))
            .padding(.horizontal, 4)
            .background(RoundedRectangle(cornerRadius: 6).fill(Color(Theme.fill)))
            .help(forAvailability ? "The shortest free time to list (− and +)" : "Meeting length (− and +)")
        }
        .padding(.horizontal, 16)
        .frame(height: 44)
    }

    @ViewBuilder private var content: some View {
        switch phase {
        case .loading:
            HStack(spacing: 8) {
                ProgressView().controlSize(.small)
                Text("Looking up busy times…").foregroundStyle(Color(Theme.text3))
            }
            .frame(maxWidth: .infinity, minHeight: 120)
        case .failed(let message):
            Text(message)
                .foregroundStyle(Color(Theme.text2))
                .frame(maxWidth: .infinity, minHeight: 120)
        case .calendarOff(let accountId, let address, let needsConsent):
            VStack(spacing: 10) {
                Text("Calendar is off for \(address)")
                    .font(.system(size: 13, weight: .semibold))
                Text("Find a time asks \(address)'s calendar provider when other people are busy. Turn calendar on for it to see them.")
                    .foregroundStyle(Color(Theme.text2))
                    .multilineTextAlignment(.center)
                    .fixedSize(horizontal: false, vertical: true)
                Button(turningOn ? (needsConsent ? "Continue in your browser…" : "Turning on…") : "Turn On Calendar") {
                    turnOn(accountId)
                }
                .disabled(turningOn)
            }
            .frame(maxWidth: 460)
            .frame(maxWidth: .infinity, minHeight: 120)
        case .ready(let info):
            grid(info)
        }
    }

    private func grid(_ info: FindTimeInfo) -> some View {
        let people = forAvailability ? info.people.filter(\.isMe) : info.people + rooms
        return VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 0) {
                Color.clear.frame(width: nameWidth, height: 1)
                GeometryReader { geo in
                    let width = geo.size.width / CGFloat(max(info.days.count, 1))
                    ForEach(Array(info.days.enumerated()), id: \.offset) { i, day in
                        VStack(alignment: .leading, spacing: 2) {
                            Text(day.title).font(.system(size: 11.5, weight: .semibold)).foregroundStyle(Color(Theme.text2))
                            HStack {
                                Text(Self.clock(day.start))
                                Spacer()
                                Text(Self.clock(day.end))
                            }
                            .font(.system(size: 10).monospacedDigit())
                            .foregroundStyle(Color(Theme.text3))
                        }
                        .padding(.horizontal, 6)
                        .frame(width: width, alignment: .leading)
                        .offset(x: CGFloat(i) * width)
                    }
                }
                .frame(height: 32)
            }
            VStack(alignment: .leading, spacing: 0) {
            ForEach(people, id: \.address) { person in
                HStack(spacing: 0) {
                    VStack(alignment: .leading, spacing: 1) {
                        HStack(spacing: 3) {
                            Text(person.name).font(.system(size: 12.5, weight: person.isMe ? .semibold : .regular)).lineLimit(1)
                            if person.isRoom {
                                Spacer(minLength: 0)
                                Button { rooms.removeAll { $0.address == person.address } } label: {
                                    Image(systemName: "xmark").font(.system(size: 8.5, weight: .semibold)).foregroundStyle(Color(Theme.text3))
                                }
                                .buttonStyle(.plain)
                                .help("Remove this room")
                                .accessibilityIdentifier("find-time-remove-room")
                            }
                        }
                        if let line = person.isRoom ? (person.note ?? person.unknown) : (person.unknown ?? person.note) {
                            Text(line).font(.system(size: 10)).foregroundStyle(Color(Theme.text3)).lineLimit(1)
                        }
                    }
                    .frame(width: nameWidth - 8, alignment: .leading)
                    .padding(.trailing, 8)
                    .help((person.unknown ?? person.note).map { "\(person.address): \($0)" } ?? person.address)
                    FindTimeRow(person: person, days: info.days)
                        .frame(height: rowHeight)
                        .help(person.unknown ?? "")
                }
                .frame(height: rowHeight + 6)
            }
            }
            .padding(.bottom, FindTimeSlots.tickRoom)
            .overlay(alignment: .topLeading) {
                FindTimeSlots(days: info.days, slots: slots, selected: forAvailability ? nil : selected)
                    .padding(.leading, nameWidth)
                    .allowsHitTesting(false)
            }
        }
        .accessibilityIdentifier("find-time-grid")
    }

    private var footer: some View {
        HStack(spacing: 12) {
            if case .ready = phase {
                if forAvailability {
                    Text(slots.isEmpty ? "No free time in the next \(Self.days) working days" : "\(slots.count) free times in the next \(Self.days) working days")
                        .foregroundStyle(Color(Theme.text2))
                } else if slots.indices.contains(selected) {
                    let slot = slots[selected]
                    Text(Self.span(slot)).font(.system(size: 12.5, weight: .medium))
                    if !slot.missing.isEmpty {
                        Text("Not known: \(slot.missing.joined(separator: ", "))")
                            .foregroundStyle(Color(Theme.text3))
                            .lineLimit(1)
                    }
                } else {
                    Text("No time in the next \(Self.days) working days suits everyone").foregroundStyle(Color(Theme.text2))
                }
            }
            Spacer(minLength: 8)
            hint("↑↓", forAvailability ? nil : "Move")
            hint("− +", "Length")
            Button { pick() } label: {
                HStack(spacing: 5) {
                    Text(pickVerb)
                    Text("↩").foregroundStyle(Color(Theme.text3))
                }
                .font(.system(size: 12, weight: .semibold))
                .padding(.horizontal, 10)
                .frame(height: 24)
                .background(RoundedRectangle(cornerRadius: 6).fill(Color(Theme.fill)))
                .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color(Theme.line), lineWidth: 1))
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .disabled(slots.isEmpty || addingRoom)
            .accessibilityIdentifier("find-time-pick")
        }
        .font(.system(size: 12))
        .padding(.horizontal, 16)
        .frame(height: 44)
    }

    @ViewBuilder private func hint(_ keys: String, _ label: String?) -> some View {
        if let label {
            HStack(spacing: 4) {
                KeyCap(key: keys)
                Text(label).foregroundStyle(Color(Theme.text3))
            }
        }
    }

    // MARK: Actions

    private func load() async {
        phase = .loading
        defer { focused = true }
        do {
            let outcome = switch request.purpose {
            case .reply(let thread): try await model.core.findTimeForThread(threadId: thread, days: Self.days)
            case .propose(let message, _): try await model.core.findTimeForInvitation(messageId: message, days: Self.days)
            case .availability(let account): try await model.core.findTimeForMe(accountId: account, days: Self.days)
            }
            switch outcome {
            case .ready(let info): phase = .ready(info); suggest()
            case .calendarOff(let id, let address, let needsConsent): phase = .calendarOff(accountId: id, address: address, needsConsent: needsConsent)
            }
        } catch {
            phase = .failed("\(error)")
        }
    }

    /// Only when someone's working hours are known, and only for meetings.
    private var showsHoursToggle: Bool {
        guard !forAvailability, case .ready(let info) = phase else { return false }
        return info.people.contains { $0.hours != nil }
    }

    private func suggest() {
        guard case .ready(let info) = phase else { return }
        slots = (forAvailability
            ? try? model.core.freeTimes(info: info, minutes: minutes)
            : try? model.core.findTimeSlots(info: withRooms(info), minutes: minutes, respectOthersHours: theirHours)) ?? []
        selected = slots.isEmpty ? 0 : min(selected, slots.count - 1)
    }

    private func move(_ delta: Int) {
        guard !slots.isEmpty, !forAvailability else { return }
        selected = max(0, min(slots.count - 1, selected + delta))
    }

    /// The next step down or up, from a meeting's own length too.
    private func length(_ delta: Int) {
        if let next = delta < 0 ? Self.lengths.last(where: { $0 < minutes }) : Self.lengths.first(where: { $0 > minutes }) {
            minutes = next
        }
    }

    private func close() {
        model.findTime = nil
    }

    private func pick() {
        guard case .ready(let info) = phase, !slots.isEmpty, !addingRoom else { return }
        let slot = slots[min(selected, slots.count - 1)]
        switch request.purpose {
        case .reply(let thread):
            close()
            model.replyProposing(thread: thread, start: slot.start, end: slot.end)
        case .propose(let message, _):
            close()
            model.proposedSlot = TimeSlot(start: slot.start, end: slot.end)
            if case .propose = model.rsvpPanel {} else { model.proposeFor = message }
        case .availability:
            guard let text = try? model.core.availabilityText(info: info, minutes: minutes) else { return }
            close()
            model.insertIntoCompose(text)
        }
    }

    private func turnOn(_ accountId: Int64) {
        guard let account = try? model.core.calendars().accounts.first(where: { $0.account.id == accountId }) else { return }
        turningOn = true
        Task {
            await CalendarSwitches(model: model).set(account, on: true).value
            turningOn = false
            await load()
        }
    }

    // MARK: Text

    private static let clockFormat: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "HH:mm"
        return f
    }()

    private static let dayFormat: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "EEE d MMM"
        return f
    }()

    static func clock(_ unix: Int64) -> String {
        clockFormat.string(from: Date(timeIntervalSince1970: TimeInterval(unix)))
    }

    /// "Tue 10 Nov, 14:00 to 14:30".
    static func span(_ slot: FindTimeSlot) -> String {
        let start = Date(timeIntervalSince1970: TimeInterval(slot.start))
        return "\(dayFormat.string(from: start)), \(clock(slot.start)) to \(clock(slot.end))"
    }

    private func lengthText(_ minutes: Int64) -> String {
        minutes < 60 ? "\(minutes) min" : minutes % 60 == 0 ? "\(minutes / 60) h" : "\(minutes / 60) h \(minutes % 60)"
    }
}

/// One person's working days side by side: busy blocks in the calendar
/// grid's look, time outside their working hours shaded, and a hatched band
/// when their times aren't known.
struct FindTimeRow: View {
    let person: PersonRow
    let days: [WorkingDay]

    var body: some View {
        Canvas { context, size in
            let width = size.width / CGFloat(max(days.count, 1))
            for (i, day) in days.enumerated() {
                let column = FindTimeSlots.column(i, of: days.count, in: size)
                context.fill(Path(roundedRect: column, cornerRadius: 5), with: .color(Color(Theme.bg)))
                if person.unknown != nil {
                    hatch(column, in: &context)
                }
                for off in person.outsideHours {
                    if let r = FindTimeSlots.span(off.start, off.end, day: day, column: column) { shade(r, column: column, in: &context) }
                }
                for b in person.busy {
                    if let r = FindTimeSlots.span(b.start, b.end, day: day, column: column) { block(r, kind: b.kind, in: &context) }
                }
                context.stroke(Path(roundedRect: column.insetBy(dx: 0.25, dy: 0.25), cornerRadius: 5), with: .color(Color(Theme.line2)), lineWidth: 0.5)
            }
        }
    }

    private func block(_ r: CGRect, kind: BusyKind, in context: inout GraphicsContext) {
        let hue = person.isRoom ? RoomBar.color : nil
        let fill = Color(CalendarColor.color(hue, .fill))
        let bar = Color(CalendarColor.color(hue, .bar))
        let box = Path(roundedRect: r, cornerRadius: 4)
        switch kind {
        case .elsewhere:
            context.stroke(Path(roundedRect: r.insetBy(dx: 0.5, dy: 0.5), cornerRadius: 3.5), with: .color(bar.opacity(0.6)), lineWidth: 1)
        case .busy, .tentative, .outOfOffice:
            context.fill(box, with: .color(kind == .outOfOffice ? bar.opacity(0.55) : fill))
            var clipped = context
            clipped.clip(to: box)
            if kind == .tentative { stripes(r, color: Color(Theme.bg), width: 3.5, in: &clipped) }
            clipped.fill(Path(CGRect(x: r.minX, y: r.minY, width: 3, height: r.height)), with: .color(bar))
        }
    }

    /// Outside working hours: a flat band with fine lines the other way from
    /// the unknown hatch, so the two never read alike.
    private func shade(_ r: CGRect, column: CGRect, in context: inout GraphicsContext) {
        var clipped = context
        clipped.clip(to: Path(roundedRect: column, cornerRadius: 5))
        clipped.fill(Path(r), with: .color(Color(Theme.fill)))
        var path = Path()
        var x = r.minX - r.height
        while x < r.maxX {
            path.move(to: CGPoint(x: x, y: r.minY))
            path.addLine(to: CGPoint(x: x + r.height, y: r.maxY))
            x += 7
        }
        clipped.clip(to: Path(r))
        clipped.stroke(path, with: .color(Color(Theme.line2)), lineWidth: 1)
    }

    private func hatch(_ r: CGRect, in context: inout GraphicsContext) {
        var clipped = context
        clipped.clip(to: Path(roundedRect: r, cornerRadius: 5))
        stripes(r, color: Color(Theme.line), width: 1, in: &clipped)
    }

    private func stripes(_ r: CGRect, color: Color, width: CGFloat, in context: inout GraphicsContext) {
        var path = Path()
        var x = r.minX
        while x < r.maxX + r.height {
            path.move(to: CGPoint(x: x, y: r.minY))
            path.addLine(to: CGPoint(x: x - r.height, y: r.maxY))
            x += 8
        }
        context.stroke(path, with: .color(color), lineWidth: width)
    }
}

/// The suggested times: the one Return picks as an accent band with a ring
/// over every row, the others as ticks under the rows so they don't read as
/// busy blocks; in Insert my availability every free time is a neutral band.
struct FindTimeSlots: View {
    let days: [WorkingDay]
    let slots: [FindTimeSlot]
    /// None in Insert my availability, where every free time goes in.
    let selected: Int?

    static let tickRoom: CGFloat = 6

    static func column(_ i: Int, of count: Int, in size: CGSize) -> CGRect {
        let width = size.width / CGFloat(max(count, 1))
        return CGRect(x: CGFloat(i) * width + 2, y: 0, width: width - 4, height: size.height)
    }

    /// Where `start` to `end` falls in a day's column, clipped to its working hours.
    static func span(_ start: Int64, _ end: Int64, day: WorkingDay, column: CGRect) -> CGRect? {
        let x = { (t: Int64) in column.minX + column.width * CGFloat(t - day.start) / CGFloat(max(day.end - day.start, 1)) }
        let (a, b) = (max(start, day.start), min(end, day.end))
        return a < b ? CGRect(x: x(a), y: column.minY, width: max(x(b) - x(a), 2), height: column.height) : nil
    }

    var body: some View {
        Canvas { context, size in
            let accent = Color(Theme.accent)
            let neutral = Color(Theme.fill)
            let tickColor = Color(Theme.text3)
            for (i, day) in days.enumerated() {
                var column = Self.column(i, of: days.count, in: size).insetBy(dx: 0, dy: 2)
                column.size.height -= Self.tickRoom
                for (n, s) in slots.enumerated() {
                    guard let r = Self.span(s.start, s.end, day: day, column: column) else { continue }
                    if selected == nil {
                        context.fill(Path(roundedRect: r, cornerRadius: 4), with: .color(neutral))
                    } else if n == selected {
                        context.fill(Path(roundedRect: r, cornerRadius: 4), with: .color(accent.opacity(0.26)))
                        context.stroke(Path(roundedRect: r.insetBy(dx: -1, dy: -1), cornerRadius: 5), with: .color(accent), lineWidth: 2)
                    } else {
                        let tick = CGRect(x: r.minX, y: r.maxY + 2, width: r.width, height: 3)
                        context.fill(Path(roundedRect: tick, cornerRadius: 1.5), with: .color(tickColor))
                    }
                }
            }
        }
    }
}
