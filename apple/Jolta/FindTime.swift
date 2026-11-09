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
        /// From compose: the user's free times go into the message, on the
        /// clock of the first address in `to`.
        case availability(account: Int64, to: String)
        /// From the event editor: the time goes back into the editor it came from.
        case editor(account: Int64, people: [PersonRecord], room: String, editor: ObjectIdentifier)
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
        findTime(for: .availability(account: account, to: compose?.to ?? ""))
    }
}

/// One panel for every Find a time: a row per person (the user first)
/// over the next working days, busy blocks drawn as the calendar draws
/// events, people whose times aren't known hatched, suggested times
/// highlighted, time outside a person's working hours shaded. ←→ or j k
/// move between suggestions, − and + change the length, O turns their
/// working hours on and off, R adds a room as one more row, Return picks,
/// ⌘Return invites everyone from a conversation, Esc closes.
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
    /// Rooms whose times are on their way; picking waits for them.
    @State private var pendingRooms = 0
    /// Insert my availability: the first recipient's clock, and their
    /// working day's off hours on it.
    @State private var theirZone: TheirZone?
    @State private var theirOff: [TimeSlot] = []
    /// Picking waits for the clock, so the times never go in on the wrong one.
    @State private var lookingUpZone = false
    /// Hold these times (H): the panel offers slots of the length, Space
    /// picks up to five, and Return inserts one line per picked time, held
    /// as tentative events once the message leaves.
    @State private var holding = false
    @State private var picked: [FindTimeSlot] = []
    /// Where the holds would go, or why they can't.
    @State private var holdChoice: HoldChoice?
    @FocusState private var focused: Bool

    private static let lengths: [Int64] = [15, 30, 45, 60, 90]
    private static let days: UInt32 = 5
    private let nameWidth: CGFloat = 132
    private let rowHeight: CGFloat = 30

    private var forAvailability: Bool {
        if case .availability = request.purpose { true } else { false }
    }

    /// Insert my availability lists free stretches unless it holds times.
    private var listsStretches: Bool { forAvailability && !holding }

    private static let maxHolds = 5

    private var title: String {
        switch request.purpose {
        case .reply: "Find a time"
        case .propose(_, let organizer): organizer.isEmpty ? "Find a new time" : "Find a new time for \(organizer)"
        case .availability: "Insert my availability"
        case .editor: "Find a time"
        }
    }

    private var pickVerb: String {
        switch request.purpose {
        case .reply: "Reply with this time"
        case .propose: "Use this time"
        case .availability: holding ? (picked.count == 1 ? "Insert 1 time" : "Insert \(picked.count) times") : "Insert these times"
        case .editor: "Use this time"
        }
    }

    /// Invite (⌘Return) opens the editor with everyone, from a conversation.
    private var invites: Bool {
        if case .reply = request.purpose { true } else { false }
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
                    add: { room in addRoom(room.address, info: info) },
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
            if forAvailability { lookingUpZone = true }
            Task { await loadZone() }
            await load()
        }
        .onChange(of: minutes) { picked = []; suggest() }
        .onChange(of: holding) { suggest() }
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
            .onKeyPress(.return, phases: .down) { press in
                if press.modifiers.contains(.command) { invite() } else { pick() }
                return .handled
            }
            .onKeyPress(keys: [.rightArrow, .downArrow, KeyEquivalent("j")]) { _ in move(1); return .handled }
            .onKeyPress(keys: [.leftArrow, .upArrow, KeyEquivalent("k")]) { _ in move(-1); return .handled }
            .onKeyPress(keys: [KeyEquivalent("-"), KeyEquivalent("_")]) { _ in length(-1); return .handled }
            .onKeyPress(keys: [KeyEquivalent("+"), KeyEquivalent("=")]) { _ in length(1); return .handled }
            .onKeyPress(keys: [KeyEquivalent("o")]) { _ in
                guard showsHoursToggle else { return .ignored }
                theirHours.toggle()
                return .handled
            }
            .onKeyPress(keys: [KeyEquivalent("h")]) { _ in
                guard forAvailability else { return .ignored }
                toggleHolding()
                return .handled
            }
            .onKeyPress(.space) {
                guard holding else { return .ignored }
                togglePick()
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
        pendingRooms += 1
        Task {
            do {
                rooms += try await model.core.roomAvailability(accountId: info.accountId, addresses: [address], from: first.start, to: last.end)
            } catch {
                model.notify("\(error)")
            }
            pendingRooms -= 1
        }
    }

    private func closeRoomPicker() {
        pickingRoom = false
        DispatchQueue.main.async { focused = true }
    }

    /// "Their time: New York · UTC−05:00 on their last mail ▾". Picking a
    /// zone remembers it for this person.
    private func zoneMenu(_ zone: TheirZone) -> some View {
        Menu {
            if let offset = zone.mailOffset, !zone.matching.isEmpty {
                Section("\(offset) on their last mail") {
                    ForEach(zone.matching, id: \.zone) { c in Button(c.city) { pickZone(c.zone, for: zone.address) } }
                }
            }
            Button("My time, \(zone.mine.city)") { pickZone(zone.mine.zone, for: zone.address) }
            Menu("All zones") {
                ForEach(zone.all, id: \.zone) { c in Button("\(c.city)   \(c.offset)") { pickZone(c.zone, for: zone.address) } }
            }
        } label: {
            HStack(spacing: 5) {
                Text("Their time:").foregroundStyle(Color(Theme.text3))
                Text(zone.zone.city).foregroundStyle(Color(Theme.text))
                Text(Self.zoneSource(zone)).foregroundStyle(Color(Theme.text3))
                Image(systemName: "chevron.down").font(.system(size: 8, weight: .semibold)).foregroundStyle(Color(Theme.text3))
            }
            .font(.system(size: 12))
            .padding(.horizontal, 8)
            .frame(height: 22)
            .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color(Theme.line), lineWidth: 1))
            .contentShape(Rectangle())
        }
        .menuStyle(.button)
        .buttonStyle(.plain)
        .menuIndicator(.hidden)
        .fixedSize()
        .help("The clock the times go into the message on, for \(zone.address)")
        .accessibilityLabel("Their time: \(zone.zone.city) \(Self.zoneSource(zone))")
        .accessibilityIdentifier("find-time-their-zone")
    }

    static func zoneSource(_ zone: TheirZone) -> String {
        switch zone.source {
        case .picked: "· picked for them"
        case .workingHours: "· from their Outlook working hours"
        case .lastMail: "· \(zone.mailOffset ?? "") on their last mail"
        case .mine: "· same as yours"
        }
    }

    private func pickZone(_ name: String, for address: String) {
        do {
            try model.core.setTheirZone(address: address, zone: name)
        } catch {
            return model.notify("\(error)")
        }
        lookingUpZone = true
        Task { await loadZone() }
        DispatchQueue.main.async { focused = true }
    }

    /// The recipient's working day under the user's row, so a time at
    /// their 03:00 stands out.
    @ViewBuilder private func theirRow(_ info: FindTimeInfo) -> some View {
        if let zone = theirZone, zone.source != .mine {
            HStack(spacing: 0) {
                VStack(alignment: .leading, spacing: 1) {
                    Text(zone.zone.city).font(.system(size: 11.5)).foregroundStyle(Color(Theme.text2)).lineLimit(1)
                    Text("Their \(Self.minutesClock(zone.hours.start)) to \(Self.minutesClock(zone.hours.end))")
                        .font(.system(size: 10)).foregroundStyle(Color(Theme.text3)).lineLimit(1)
                }
                .frame(width: nameWidth - 8, alignment: .leading)
                .padding(.trailing, 8)
                FindTimeRow(person: PersonRow(address: zone.address, name: "Their day", isMe: false, isRoom: false, busy: [], unknown: nil,
                                              hours: zone.hours, outsideHours: theirOff, note: nil),
                            days: info.days)
                    .frame(height: 12)
            }
            .frame(height: 26)
            .help("\(zone.address)'s working day on their clock")
            .accessibilityElement(children: .combine)
        }
    }

    static func minutesClock(_ minutes: UInt32) -> String {
        String(format: "%02d:%02d", minutes / 60, minutes % 60)
    }

    private var header: some View {
        HStack(spacing: 10) {
            Image(systemName: "calendar.badge.clock").foregroundStyle(Color(Theme.text3))
            Text(title).font(.system(size: 13, weight: .semibold))
            Spacer()
            if forAvailability, let zone = theirZone {
                zoneMenu(zone)
            }
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
                FindTimeSlots(days: info.days, slots: slots, selected: listsStretches ? nil : selected,
                              picked: holding ? Set(picked.map(\.start)) : nil)
                    .padding(.leading, nameWidth)
                    .allowsHitTesting(false)
            }
            if forAvailability { theirRow(info) }
        }
        .accessibilityIdentifier("find-time-grid")
    }

    private var footer: some View {
        HStack(spacing: 12) {
            if case .ready = phase {
                if holding {
                    Text(picked.isEmpty ? "Pick up to \(Self.maxHolds) times" : picked.count == 1 ? "1 time picked" : "\(picked.count) times picked")
                        .foregroundStyle(Color(Theme.text2))
                } else if forAvailability {
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
            if forAvailability { holdToggle }
            Spacer(minLength: 8)
            hint("↑↓", listsStretches ? nil : "Move")
            hint("Space", holding ? "Pick" : nil)
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
            .disabled(slots.isEmpty || pendingRooms > 0 || lookingUpZone || (holding && picked.isEmpty))
            .accessibilityIdentifier("find-time-pick")
            if invites {
                Button { invite() } label: {
                    HStack(spacing: 5) {
                        Text("Invite")
                        Text("⌘↩").opacity(0.75)
                    }
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(Color(Theme.raise))
                    .padding(.horizontal, 10)
                    .frame(height: 24)
                    .background(RoundedRectangle(cornerRadius: 6).fill(Color(Theme.accent)))
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .keyboardShortcut(.return, modifiers: .command)
                .disabled(slots.isEmpty || pendingRooms > 0)
                .help("Open the event editor with this time, everyone here and the room (⌘↩)")
                .accessibilityIdentifier("find-time-invite")
            }
        }
        .font(.system(size: 12))
        .padding(.horizontal, 16)
        .frame(height: 44)
    }

    /// "Hold these times" with H, or why the account's times can't be held.
    private var holdToggle: some View {
        let reason = holdChoice?.reason
        return HStack(spacing: 6) {
            Toggle("Hold these times", isOn: Binding(get: { holding }, set: { _ in toggleHolding() }))
                .toggleStyle(.switch)
                .controlSize(.mini)
                .labelsHidden()
                .disabled(holdChoice == nil || reason != nil)
                .accessibilityIdentifier("find-time-hold")
            Text("Hold these times").foregroundStyle(Color(reason == nil ? Theme.text2 : Theme.text3))
            KeyCap(key: "H")
        }
        .fixedSize()
        .help(reason ?? "Hold the picked times as tentative events in \(holdChoice?.calendar ?? "your calendar") once the message leaves, until someone books one (H)")
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
        if case .availability(let account, _) = request.purpose {
            holdChoice = try? model.core.holdChoice(accountId: account)
        }
        defer { focused = true }
        do {
            let outcome = switch request.purpose {
            case .reply(let thread): try await model.core.findTimeForThread(threadId: thread, days: Self.days)
            case .propose(let message, _): try await model.core.findTimeForInvitation(messageId: message, days: Self.days)
            case .availability(let account, _): try await model.core.findTimeForMe(accountId: account, days: Self.days)
            case .editor(let account, let people, let room, _):
                try await model.core.findTimeForPeople(accountId: account, people: people, rooms: room.isEmpty ? [] : [room], days: Self.days)
            }
            switch outcome {
            case .ready(var info):
                // The editor's room is a row like one added with R, so it can be removed the same way.
                rooms = info.people.filter(\.isRoom)
                info.people.removeAll(where: \.isRoom)
                phase = .ready(info)
                shadeTheirDay()
                suggest()
            case .calendarOff(let id, let address, let needsConsent): phase = .calendarOff(accountId: id, address: address, needsConsent: needsConsent)
            }
        } catch {
            phase = .failed("\(error)")
        }
    }

    /// The first recipient's clock, for Insert my availability. Without it
    /// the times go in on this Mac's clock, as before.
    private func loadZone() async {
        guard case .availability(let account, let to) = request.purpose else { return }
        defer { lookingUpZone = false }
        theirZone = try? await model.core.theirZone(accountId: account, to: to)
        shadeTheirDay()
    }

    private func shadeTheirDay() {
        guard case .ready(let info) = phase, let zone = theirZone else { return theirOff = [] }
        theirOff = (try? model.core.outsideHours(info: info, hours: zone.hours)) ?? []
    }

    /// Only when someone's working hours are known, and only for meetings.
    private var showsHoursToggle: Bool {
        guard !forAvailability, case .ready(let info) = phase else { return false }
        return info.people.contains { $0.hours != nil }
    }

    private func suggest() {
        guard case .ready(let info) = phase else { return }
        slots = (listsStretches
            ? try? model.core.freeTimes(info: info, minutes: minutes)
            : try? model.core.findTimeSlots(info: withRooms(info), minutes: minutes, respectOthersHours: forAvailability ? false : theirHours)) ?? []
        selected = slots.isEmpty ? 0 : min(selected, slots.count - 1)
    }

    private func move(_ delta: Int) {
        guard !slots.isEmpty, !listsStretches else { return }
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

    private func toggleHolding() {
        guard holdChoice?.reason == nil, holdChoice != nil else {
            return model.notify(holdChoice?.reason ?? "Jolta can't hold times for this account")
        }
        holding.toggle()
        picked = []
        selected = 0
    }

    /// Space: picks the time under the cursor, or drops it.
    private func togglePick() {
        guard slots.indices.contains(selected) else { return }
        let slot = slots[selected]
        if let i = picked.firstIndex(where: { $0.start == slot.start }) {
            picked.remove(at: i)
        } else if picked.count < Self.maxHolds {
            picked.append(slot)
            picked.sort { $0.start < $1.start }
        } else {
            model.notify("Jolta holds up to \(Self.maxHolds) times for one message")
        }
    }

    private func pick() {
        guard case .ready(let info) = phase, !slots.isEmpty, pendingRooms == 0, !lookingUpZone else { return }
        let slot = slots[min(selected, slots.count - 1)]
        switch request.purpose {
        case .reply(let thread):
            close()
            model.replyProposing(thread: thread, start: slot.start, end: slot.end)
        case .propose(let message, _):
            close()
            model.proposedSlot = TimeSlot(start: slot.start, end: slot.end)
            if case .propose = model.rsvpPanel {} else { model.proposeFor = message }
        case .availability(let account, _) where holding:
            guard !picked.isEmpty else { return }
            close()
            model.insertHeldTimes(picked.map { TimeSlot(start: $0.start, end: $0.end) }, account: account, zone: theirZone?.zone.zone)
        case .availability:
            guard let text = try? model.core.availabilityText(info: info, minutes: minutes, zone: theirZone?.zone.zone) else { return }
            close()
            model.insertIntoCompose(text)
        case .editor(_, _, _, let editor):
            close()
            model.calendar.useFoundTime(in: editor, start: slot.start, end: slot.end, room: rooms.first)
        }
    }

    /// ⌘Return: the event editor with the time, everyone on the rows as
    /// Required and the first room, titled after the conversation.
    private func invite() {
        guard invites, case .reply(let thread) = request.purpose, case .ready(let info) = phase, !slots.isEmpty, pendingRooms == 0 else { return }
        let slot = slots[min(selected, slots.count - 1)]
        let record = { (row: PersonRow) in PersonRecord(name: row.name, address: row.address) }
        do {
            let draft = try model.core.inviteDraft(accountId: info.accountId, threadId: thread, people: info.people.filter { !$0.isMe && !$0.isRoom }.map(record),
                                                   rooms: rooms.map(record), start: slot.start, end: slot.end)
            close()
            model.openInvite(draft)
        } catch {
            model.notify("\(error)")
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
    /// Hold these times: the starts of the picked slots. Every slot is a
    /// candidate box, the picked ones tinted and ticked, the cursor ringed.
    var picked: Set<Int64>? = nil

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
                    if let picked {
                        let box = Path(roundedRect: r, cornerRadius: 4)
                        let on = picked.contains(s.start)
                        context.fill(box, with: .color(on ? accent.opacity(0.2) : neutral))
                        context.stroke(Path(roundedRect: r.insetBy(dx: 0.5, dy: 0.5), cornerRadius: 3.5), with: .color(on ? accent : Color(Theme.line)), lineWidth: on ? 1.5 : 1)
                        if on {
                            context.draw(Text("✓").font(.system(size: 10, weight: .bold)).foregroundStyle(accent), at: CGPoint(x: r.midX, y: r.midY))
                        }
                        if n == selected {
                            context.stroke(Path(roundedRect: r.insetBy(dx: -2, dy: -2), cornerRadius: 5.5), with: .color(accent), lineWidth: 2)
                        }
                    } else if selected == nil {
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
