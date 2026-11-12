@preconcurrency import JoltaFFI
import AppKit
import SwiftUI

/// The room picker: the account's rooms grouped by room list, recent rooms
/// from the user's own meetings, and a typed address. Each row has a bar of
/// the hours around the meeting with its time outlined. On Microsoft 365 it
/// asks for the room lists the first time it opens on an account; without
/// them, recent rooms and a typed address still work. ↑↓ move, Return adds,
/// Esc closes.
struct RoomPicker: View {
    let core: Core
    let accountId: Int64
    /// The meeting's time, when one is picked; the Free filter needs it.
    let slot: FindTimeSlot?
    /// Where the bars start when no time is picked.
    let fallbackStart: Int64
    let people: Int
    /// Addresses already on the meeting, left out of the list.
    let taken: Set<String>
    let add: (RoomInfo) -> Void
    let close: () -> Void

    @State private var outcome: RoomsOutcome?
    @State private var query = ""
    @State private var freeOnly = true
    @State private var minimum: UInt32 = 0
    @State private var selected = 0
    @State private var busy: [String: PersonRow] = [:]
    @State private var asking: String?
    @State private var failure: String?
    @FocusState private var focused: Bool

    /// Asked once per account and launch, so closing the browser doesn't
    /// bring it back each time the picker opens.
    @MainActor private static var asked: Set<Int64> = []
    private static let sizes: [UInt32] = [0, 2, 4, 6, 8, 12, 20]

    private var bar: (start: Int64, end: Int64) {
        let start = slot?.start ?? fallbackStart
        let hour = Calendar.current.dateInterval(of: .hour, for: Date(timeIntervalSince1970: TimeInterval(start)))?.start ?? Date(timeIntervalSince1970: TimeInterval(start))
        let from = Int64(hour.timeIntervalSince1970) - 2 * 3600
        return (from, from + 5 * 3600)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            search
            Rectangle().fill(Color(Theme.line2)).frame(height: 1)
            filters
            notice
            list
            Rectangle().fill(Color(Theme.line2)).frame(height: 1)
            footer
        }
        .frame(width: 440)
        .panel()
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("room-picker")
        .task { await load() }
        .onChange(of: query) { selected = 0; Task { await lookUpTyped() } }
        .onChange(of: freeOnly) { selected = 0 }
        .onChange(of: minimum) { selected = 0 }
    }

    private var search: some View {
        HStack(spacing: 8) {
            Image(systemName: "door.left.hand.open").foregroundStyle(Color(Theme.text3))
            TextField("Search rooms or type a room's address", text: $query)
                .textFieldStyle(.plain)
                .font(.system(size: 13))
                .focused($focused)
                .onAppear { DispatchQueue.main.async { focused = true } }
                .onKeyPress(.escape) { close(); return .handled }
                .onKeyPress(.return) { choose(); return .handled }
                .onKeyPress(.downArrow) { move(1); return .handled }
                .onKeyPress(.upArrow) { move(-1); return .handled }
                .accessibilityIdentifier("room-picker-search")
        }
        .padding(.horizontal, 12)
        .frame(height: 38)
    }

    private var filters: some View {
        HStack(spacing: 8) {
            if let slot {
                chip("Free at \(FindTimePanel.clock(slot.start)) to \(FindTimePanel.clock(slot.end))", on: freeOnly) { freeOnly.toggle() }
                    .accessibilityIdentifier("room-picker-free")
            }
            Menu {
                ForEach(Self.sizes, id: \.self) { n in
                    Button(n == 0 ? "Any size" : "\(n)+ people") { minimum = n }
                }
            } label: {
                Text(minimum == 0 ? "Any size" : "\(minimum)+ people")
            }
            .menuStyle(.borderlessButton)
            .fixedSize()
            .font(.system(size: Theme.small))
            .accessibilityIdentifier("room-picker-size")
            Spacer()
            if outcome == nil || asking != nil { ProgressView().controlSize(.small) }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }

    private func chip(_ text: String, on: Bool, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            HStack(spacing: 4) {
                Text(text)
                if on { Image(systemName: "checkmark").font(.system(size: 9, weight: .bold)) }
            }
            .font(.system(size: Theme.small))
            .foregroundStyle(Color(on ? Theme.text : Theme.text2))
            .padding(.horizontal, 8)
            .frame(height: Theme.controlSmall)
            .background(RoundedRectangle(cornerRadius: 6).fill(Color(on ? Theme.fill : .clear)))
            .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color(Theme.line), lineWidth: 1))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    /// What the account allows, when it changes what the list can hold.
    @ViewBuilder private var notice: some View {
        if let asking {
            message(asking, nil, action: nil)
        } else if let failure {
            message("Couldn't read the rooms", failure, action: ("Try Again", { self.failure = nil; Task { await load() } }))
        } else if let outcome {
            switch outcome.access {
            case .refused(let reason):
                message("Room lists need your admin's OK",
                        "Your organization lets only an admin allow Jolta to read its rooms (permission \"Read all company places\"). Microsoft said: \(reason.hasSuffix(".") ? reason : reason + ".")",
                        action: ("Ask for Access", { Task { await ask() } }))
            case .notAsked:
                message("Room lists need your OK", "Jolta asks Microsoft to read your organization's rooms, with their size and building.",
                        action: ("Ask for Access", { Task { await ask() } }))
            case .granted where outcome.noneListed:
                message("Your organization hasn't set up room lists", "Rooms from your meetings and a typed address still work.", action: nil)
            case .granted, .notOffered:
                EmptyView()
            }
        }
    }

    private func message(_ title: String, _ text: String?, action: (String, () -> Void)?) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(title).font(.system(size: Theme.body, weight: .semibold)).foregroundStyle(Color(Theme.text))
            if let text {
                Text(text).font(.system(size: 12)).foregroundStyle(Color(Theme.text2)).fixedSize(horizontal: false, vertical: true)
            }
            if let action {
                KitButton(title: action.0, size: .small, action: action.1)
                    .padding(.top, 3)
                    .accessibilityIdentifier("room-picker-ask")
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 8)
        .frame(maxWidth: .infinity, alignment: .leading)
        .accessibilityIdentifier("room-picker-notice")
    }

    // MARK: The list

    private struct Group: Identifiable {
        let id: String
        let title: String
        let rooms: [RoomInfo]
    }

    /// The rooms that pass the search and the filters, by room list, then
    /// the rooms in no list, then recent rooms, as the mockup has them.
    private var groups: [Group] {
        let q = query.trimmingCharacters(in: .whitespaces).lowercased()
        let shown = (outcome?.rooms ?? []).filter { r in
            !taken.contains(r.address.lowercased())
                && (q.isEmpty || r.name.lowercased().contains(q) || r.address.contains(q) || (r.place?.lowercased().contains(q) ?? false))
                && (minimum == 0 || r.capacity.map { $0 >= minimum } ?? true)
                && (!freeOnly || slot == nil || isFree(r.address))
        }
        var out: [Group] = []
        let listed = shown.filter { $0.origin == .directory }
        for name in listed.compactMap(\.list).reduce(into: [String]()) { if !$0.contains($1) { $0.append($1) } } {
            out.append(Group(id: "list-\(name)", title: name.uppercased(), rooms: listed.filter { $0.list == name }))
        }
        let unlisted = listed.filter { $0.list == nil }
        if !unlisted.isEmpty { out.append(Group(id: "unlisted", title: out.isEmpty ? "ROOMS" : "OTHER ROOMS", rooms: unlisted)) }
        let recent = shown.filter { $0.origin == .recent }
        if !recent.isEmpty { out.append(Group(id: "recent", title: out.isEmpty ? "RECENT ROOMS" : "RECENT", rooms: recent)) }
        if let typed { out.append(Group(id: "typed", title: "TYPED ADDRESS", rooms: [typed])) }
        return out
    }

    /// The search as a room address, when it is one no listed room has.
    private var typed: RoomInfo? {
        let q = query.trimmingCharacters(in: .whitespaces).lowercased()
        guard Self.isAddress(q), !taken.contains(q), !(outcome?.rooms ?? []).contains(where: { $0.address == q }) else { return nil }
        return RoomInfo(address: q, name: q, capacity: nil, list: nil, place: nil, equipment: nil, accessible: false, origin: .typed)
    }

    private var flat: [RoomInfo] { groups.flatMap(\.rooms) }

    @ViewBuilder private var list: some View {
        let rooms = flat
        if rooms.isEmpty, outcome != nil {
            Text(emptyText)
                .font(.system(size: 12))
                .foregroundStyle(Color(Theme.text3))
                .padding(.horizontal, 14)
                .padding(.vertical, 16)
                .frame(maxWidth: .infinity, alignment: .leading)
        } else {
            ScrollViewReader { proxy in
                ScrollView {
                    VStack(alignment: .leading, spacing: 0) {
                        ForEach(groups) { g in
                            Text(g.title)
                                .font(.system(size: 11, weight: .semibold))
                                .foregroundStyle(Color(Theme.text3))
                                .padding(.horizontal, 12)
                                .padding(.top, 10)
                                .padding(.bottom, 4)
                            ForEach(g.rooms, id: \.address) { r in
                                row(r, group: g.id, current: rooms.indices.contains(selected) && rooms[selected].address == r.address)
                                    .id(r.address)
                            }
                        }
                    }
                    .padding(.bottom, 6)
                }
                .frame(maxHeight: 300)
                .fixedSize(horizontal: false, vertical: true)
                .onChange(of: selected) { if rooms.indices.contains(selected) { proxy.scrollTo(rooms[selected].address) } }
            }
        }
    }

    private var emptyText: String {
        if !(outcome?.rooms.isEmpty ?? true) { return "No room fits. Change the filters, or type a room's address." }
        return "No rooms yet: none on your meetings of the last 180 days. Type a room's address."
    }

    private func row(_ r: RoomInfo, group: String, current: Bool) -> some View {
        let state = busy[r.address]
        let taken = slot.map { !isFree(r.address, at: $0) } ?? false
        let detail: String = {
            if r.origin == .typed { return state?.unknown ?? "Typed address" }
            if taken, let slot { return "Busy at \(FindTimePanel.clock(slot.start))" }
            if group.hasPrefix("list") || group == "unlisted" { return r.equipment ?? r.place ?? r.address }
            return r.place ?? "From your meetings"
        }()
        return Button { add(r) } label: {
            HStack(spacing: 10) {
                VStack(alignment: .leading, spacing: 1) {
                    Text(r.name).font(.system(size: Theme.body)).lineLimit(1)
                    Text(detail).font(.system(size: 11)).foregroundStyle(Color(Theme.text3)).lineLimit(1)
                }
                Spacer(minLength: 6)
                Text(r.capacity.map { "\($0)" } ?? "–")
                    .font(.system(size: 11, weight: .medium).monospacedDigit())
                    .foregroundStyle(Color(Theme.text3))
                    .frame(width: 30, alignment: .trailing)
                    .help(r.capacity.map { "Seats \($0)" } ?? "Size not known")
                RoomBar(row: state, from: bar.start, to: bar.end, slot: slot)
                    .frame(width: 120, height: 14)
            }
            .foregroundStyle(Color(taken ? Theme.text3 : Theme.text))
            .padding(.horizontal, 12)
            .padding(.vertical, 5)
            .selectedRow(current, radius: 0)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(r.address)
        .accessibilityIdentifier("room-row-\(r.address)")
    }

    private var footer: some View {
        HStack(spacing: 12) {
            HStack(spacing: 4) { KeyCap(key: "↑↓"); Text("move") }
            HStack(spacing: 4) { KeyCap(key: "↩"); Text("add room") }
            Spacer()
            Text("Bars show \(FindTimePanel.clock(bar.start)) to \(FindTimePanel.clock(bar.end))")
        }
        .font(.system(size: Theme.small))
        .foregroundStyle(Color(Theme.text3))
        .padding(.horizontal, 12)
        .frame(height: 32)
    }

    // MARK: Actions

    private func isFree(_ address: String) -> Bool {
        guard let slot else { return true }
        return isFree(address, at: slot)
    }

    /// A room whose times aren't known stays in the list.
    private func isFree(_ address: String, at slot: FindTimeSlot) -> Bool {
        guard let row = busy[address] else { return true }
        return !row.busy.contains { $0.kind != .elsewhere && $0.start < slot.end && $0.end > slot.start }
    }

    private func load() async {
        do {
            var now = try core.rooms(accountId: accountId)
            outcome = now
            await lookUp(now.rooms.map(\.address))
            if case .notAsked = now.access, !Self.asked.contains(accountId) {
                await ask()
                return
            }
            if now.stale {
                now = try await core.refreshRooms(accountId: accountId)
                outcome = now
                await lookUp(now.rooms.map(\.address))
            }
        } catch {
            failure = "\(error)"
        }
    }

    /// Opens Microsoft's consent page, where an admin allows the room lists
    /// or a user is told to ask one; the answer is recorded either way.
    private func ask() async {
        Self.asked.insert(accountId)
        asking = "Continue in your browser: Microsoft asks whether Jolta may read your organization's rooms."
        failure = nil
        defer { asking = nil }
        do {
            let session = try await core.beginRoomsConsent(accountId: accountId)
            if let url = URL(string: session.url()) { NSWorkspace.shared.open(url) }
            _ = try? await session.finish()
            let now = try core.rooms(accountId: accountId)
            outcome = now
            await lookUp(now.rooms.map(\.address))
        } catch {
            failure = "\(error)"
        }
    }

    private func lookUp(_ addresses: [String]) async {
        let missing = addresses.filter { busy[$0] == nil }
        guard !missing.isEmpty else { return }
        let rows = (try? await core.roomAvailability(accountId: accountId, addresses: missing, from: bar.start, to: bar.end)) ?? []
        for r in rows { busy[r.address] = r }
    }

    private func lookUpTyped() async {
        guard let typed else { return }
        try? await Task.sleep(for: .milliseconds(350))
        guard self.typed?.address == typed.address else { return }
        await lookUp([typed.address])
    }

    private func move(_ delta: Int) {
        let count = flat.count
        guard count > 0 else { return }
        selected = max(0, min(count - 1, selected + delta))
    }

    private func choose() {
        let rooms = flat
        guard rooms.indices.contains(selected) else { return }
        add(rooms[selected])
    }

    static func isAddress(_ s: String) -> Bool {
        let parts = s.split(separator: "@", omittingEmptySubsequences: false)
        return parts.count == 2 && !parts[0].isEmpty && parts[1].contains(".") && !s.contains(" ")
    }
}

/// A room's busy times over the hours around the meeting, with the
/// meeting's time outlined; hatched when they aren't known.
struct RoomBar: View {
    let row: PersonRow?
    let from: Int64
    let to: Int64
    let slot: FindTimeSlot?

    var body: some View {
        Canvas { context, size in
            let r = CGRect(origin: .zero, size: size)
            context.fill(Path(roundedRect: r, cornerRadius: 4), with: .color(Color(Theme.bg)))
            let x = { (t: Int64) in size.width * CGFloat(t - from) / CGFloat(max(to - from, 1)) }
            if row?.unknown != nil {
                var clipped = context
                clipped.clip(to: Path(roundedRect: r, cornerRadius: 4))
                var path = Path()
                var at: CGFloat = 0
                while at < size.width + size.height {
                    path.move(to: CGPoint(x: at, y: 0))
                    path.addLine(to: CGPoint(x: at - size.height, y: size.height))
                    at += 6
                }
                clipped.stroke(path, with: .color(Color(Theme.line)), lineWidth: 1)
            }
            for b in row?.busy ?? [] where b.end > from && b.start < to {
                let (a, e) = (x(max(b.start, from)), x(min(b.end, to)))
                let block = CGRect(x: a, y: 2, width: max(e - a, 2), height: size.height - 4)
                context.fill(Path(roundedRect: block, cornerRadius: 2), with: .color(Color(CalendarColor.color(RoomBar.color, .allDay))))
            }
            if let slot, slot.end > from, slot.start < to {
                let (a, e) = (x(max(slot.start, from)), x(min(slot.end, to)))
                let mark = CGRect(x: a, y: 0, width: max(e - a, 2), height: size.height)
                context.fill(Path(mark), with: .color(Color(Theme.accent).opacity(0.14)))
                context.stroke(Path(CGRect(x: mark.minX, y: 0, width: 0, height: size.height)), with: .color(Color(Theme.accent)), lineWidth: 1.5)
                context.stroke(Path(CGRect(x: mark.maxX, y: 0, width: 0, height: size.height)), with: .color(Color(Theme.accent)), lineWidth: 1.5)
            }
            context.stroke(Path(roundedRect: r.insetBy(dx: 0.25, dy: 0.25), cornerRadius: 4), with: .color(Color(Theme.line)), lineWidth: 0.5)
        }
        .help(row?.unknown ?? "")
    }

    /// Rooms draw in their own color, apart from people's calendars.
    static let color = "#3f8f6b"
}
