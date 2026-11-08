@preconcurrency import JoltaFFI
import AppKit
import SwiftUI

/// Turning calendar on per account. IMAP accounts turn on at once (CalDAV
/// uses the mail password); Google and Microsoft accounts first grant
/// calendar access in the browser, the same sign-in as adding an account.
@MainActor
@Observable
final class CalendarSwitches {
    let model: AppModel
    /// Accounts being turned on, with what to tell the user meanwhile.
    private(set) var working: [Int64: String] = [:]
    private(set) var messages: [Int64: (text: String, failed: Bool)] = [:]

    init(model: AppModel) { self.model = model }

    @discardableResult
    func set(_ account: CalendarAccount, on: Bool) -> Task<Void, Never> {
        let id = account.account.id
        messages[id] = nil
        let core = model.core
        working[id] = account.needsConsent && on ? "Continue in your browser…" : on ? "Finding calendars…" : "Turning off…"
        return Task {
            do {
                if on, account.needsConsent {
                    let session = try await core.beginCalendarConsent(accountId: id)
                    if let url = URL(string: session.url()) { NSWorkspace.shared.open(url) }
                    _ = try await session.finish()
                } else {
                    _ = try await core.setCalendarEnabled(accountId: id, on: on)
                }
                if on { messages[id] = ("Calendar is on. Jolta is syncing it.", false) }
            } catch {
                messages[id] = ("\(error)", true)
            }
            working[id] = nil
            model.calendar.reloadCalendars()
            model.calendar.refresh()
        }
    }
}

/// Every account with its Calendar switch.
struct CalendarAccountRows: View {
    let model: AppModel
    let switches: CalendarSwitches

    var body: some View {
        let list = model.calendar.list
        ForEach(list.accounts, id: \.account.id) { a in
            let id = a.account.id
            let message = switches.messages[id]
            SettingsRow(title: a.account.address,
                        detail: switches.working[id] ?? message?.text ?? Self.detail(a),
                        detailColor: message?.failed == true ? .red : Color(Theme.text3)) {
                HStack(spacing: 8) {
                    if switches.working[id] != nil { ProgressView().controlSize(.small) }
                    Toggle("Calendar", isOn: Binding(get: { a.calendarOn }, set: { switches.set(a, on: $0) }))
                        .settingsSwitch()
                        .disabled(switches.working[id] != nil)
                        .accessibilityLabel("Calendar for \(a.account.address)")
                }
            }
        }
        if list.accounts.isEmpty {
            SettingsRow("No accounts yet", detail: "Add an account in Settings, or subscribe to a calendar below.")
        }
    }

    static func detail(_ a: CalendarAccount) -> String {
        if a.calendarOn {
            let shown = a.calendars.filter(\.shown).count
            return "\(a.account.provider). \(shown) of \(a.calendars.count) calendars shown."
        }
        return a.needsConsent
            ? "\(a.account.provider). Turning it on asks \(a.account.kind == "gmail" ? "Google" : "Microsoft") for calendar access in your browser."
            : "\(a.account.provider). Uses the account's password for CalDAV."
    }
}

/// A `webcal://` or `https://` address, downloaded before it is added.
struct SubscribeField: View {
    let model: AppModel
    @State private var url = ""
    @State private var adding = false
    @State private var message: (text: String, failed: Bool)?

    var body: some View {
        SettingsRow(title: "Add a calendar subscription",
                    detail: message?.text ?? "A feed address such as webcal://… from a holiday or sports calendar. Jolta checks it for changes every few hours.",
                    detailColor: message?.failed == true ? .red : Color(Theme.text3)) {
            HStack(spacing: 8) {
                TextField("", text: $url, prompt: Text("webcal://"))
                    .settingsField(width: 190)
                    .onSubmit(add)
                Button(adding ? "Adding…" : "Add", action: add)
                    .fixedSize()
                    .disabled(adding || url.trimmingCharacters(in: .whitespaces).isEmpty)
            }
        }
    }

    private func add() {
        let address = url.trimmingCharacters(in: .whitespaces)
        guard !address.isEmpty, !adding else { return }
        adding = true
        message = nil
        Task {
            do {
                let info = try await model.core.subscribeCalendar(url: address)
                message = info.syncError.map { ("Added \(info.name), but the first download failed: \($0)", true) }
                    ?? ("Added \(info.name) with \(info.events) events.", false)
                url = ""
            } catch {
                message = ("\(error)", true)
            }
            adding = false
            model.calendar.reloadCalendars()
            model.calendar.refresh()
        }
    }
}

/// A calendar kept only in Jolta's store, with a name and a color.
struct NewLocalCalendarField: View {
    let model: AppModel
    @State private var name = ""
    @State private var color = CalendarSettingsRow.swatches[0]
    @State private var message: (text: String, failed: Bool)?

    var body: some View {
        SettingsRow(title: "New calendar on this Mac",
                    detail: message?.text ?? "For private plans, or when you have no calendar account.",
                    detailColor: message?.failed == true ? .red : Color(Theme.text3)) {
            HStack(spacing: 8) {
                Menu {
                    ForEach(CalendarSettingsRow.swatches, id: \.self) { hex in
                        Button { color = hex } label: {
                            Label { Text(hex) } icon: { Image(nsImage: CalendarSettingsRow.swatch(hex)) }
                        }
                    }
                } label: {
                    RoundedRectangle(cornerRadius: 3).fill(Color(CalendarColor.color(color, .bar))).frame(width: 12, height: 12)
                }
                .menuStyle(.button)
                .buttonStyle(.plain)
                .menuIndicator(.hidden)
                .fixedSize()
                .help("Color")
                .accessibilityLabel("Color")
                TextField("", text: $name, prompt: Text("Name"))
                    .settingsField(width: 150)
                    .onSubmit(add)
                    .accessibilityLabel("Calendar name")
                    .accessibilityIdentifier("new-calendar-name")
                Button("Create", action: add)
                    .fixedSize()
                    .disabled(name.trimmingCharacters(in: .whitespaces).isEmpty)
            }
        }
    }

    private func add() {
        let title = name.trimmingCharacters(in: .whitespaces)
        guard !title.isEmpty else { return }
        do {
            let info = try model.core.newCalendarHere(name: title, color: color)
            message = ("Made \(info.name).", false)
            name = ""
        } catch {
            message = ("\(error)", true)
        }
        model.calendar.reloadCalendars()
        model.calendar.refresh()
    }
}

/// Settings › Calendar: accounts' switches, each calendar's settings, subscriptions.
struct CalendarSettings: View {
    static let localFooter = "Events in a calendar on this Mac stay here: they don't reach your phone or any server."
    let model: AppModel
    @State private var switches: CalendarSwitches?
    @State private var removal: Confirmation?

    var body: some View {
        let cal = model.calendar
        SettingsScroll {
            if let switches {
                SettingsSection("Accounts", footer: "Calendar is off for every account until you turn it on. Turning it off removes the account's calendars from Jolta; the server keeps them.") {
                    CalendarAccountRows(model: model, switches: switches)
                }
            }
            ForEach(cal.groups) { group in
                SettingsSection(group.title, footer: group.id == "feeds" ? "Subscriptions are read-only." : group.id == "local" ? Self.localFooter : nil) {
                    ForEach(group.calendars) { entry in CalendarSettingsRow(model: model, info: entry.info, removal: $removal) }
                    if group.id == "local" { NewLocalCalendarField(model: model) }
                }
            }
            if !cal.groups.contains(where: { $0.id == "local" }) {
                SettingsSection("On this Mac only", footer: Self.localFooter) { NewLocalCalendarField(model: model) }
            }
            SettingsSection("New events") {
                NewEventDefaults(model: model)
            }
            SettingsSection("Invitations") {
                FileResponsesRow(core: model.core)
            }
            SettingsSection("Out of office", footer: "Jolta shows a line over the mail list while automatic replies are on. It can turn them off on Microsoft 365. Gmail's vacation responder is changed in Gmail.") {
                OutOfOfficeRows(model: model, switches: switches)
            }
            SettingsSection("Working hours", footer: "Find a time and Insert my availability suggest times inside these hours. Until you set them, Jolta uses your Outlook working hours on Microsoft 365, else Monday to Friday, 9:00 to 17:00.") {
                WorkingHoursRow(core: model.core)
            }
            SettingsSection("Subscribe") {
                SubscribeField(model: model)
                SettingsRow(title: "Open webcal:// links in Jolta",
                            detail: "Clicking a calendar subscription link anywhere on this Mac subscribes in Jolta.") {
                    Button("Use Jolta") {
                        NSWorkspace.shared.setDefaultApplication(at: Bundle.main.bundleURL, toOpenURLsWithScheme: "webcal") { _ in }
                    }
                }
            }
        }
        .confirmation($removal)
        .onAppear {
            if switches == nil { switches = CalendarSwitches(model: model) }
            cal.reloadCalendars()
        }
    }
}

/// The days and hours Find a time suggests times in, on this Mac's clock.
struct WorkingHoursRow: View {
    let core: Core
    @State private var hours: WorkingHoursInfo?
    @State private var custom = false
    @State private var failure: String?

    private static let letters = ["M", "T", "W", "T", "F", "S", "S"]

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            if let hours {
                HStack(spacing: 4) {
                    ForEach(0..<7, id: \.self) { i in
                        let on = hours.days.indices.contains(i) && hours.days[i]
                        Button(Self.letters[i]) { toggle(day: i) }
                            .buttonStyle(.plain)
                            .font(.system(size: 12, weight: .semibold))
                            .frame(width: 26, height: 24)
                            .background(RoundedRectangle(cornerRadius: 6).fill(on ? Color(Theme.fill) : .clear))
                            .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color(Theme.line), lineWidth: 1))
                            .foregroundStyle(Color(on ? Theme.text : Theme.text3))
                            .accessibilityLabel(Calendar.current.weekdaySymbols[(i + 1) % 7])
                            .accessibilityValue(on ? "on" : "off")
                    }
                    Spacer(minLength: 16)
                    DatePicker("From", selection: time(\.start), displayedComponents: .hourAndMinute)
                    Text("to").foregroundStyle(Color(Theme.text3))
                    DatePicker("To", selection: time(\.end), displayedComponents: .hourAndMinute)
                }
                .labelsHidden()
                .datePickerStyle(.field)
                HStack {
                    Text(failure ?? (custom ? "Set here, in \(hours.zone)." : "Not set; these are the defaults, in \(hours.zone)."))
                        .font(.system(size: 12))
                        .foregroundStyle(failure == nil ? Color(Theme.text3) : .red)
                    Spacer()
                    if custom { Button("Use Defaults") { reset() } }
                }
            }
        }
        .padding(.vertical, 4)
        .onAppear(perform: load)
    }

    private func load() {
        let set = try? core.workingHours()
        custom = set != nil
        hours = set ?? core.standardWorkingHours()
    }

    private func save(_ new: WorkingHoursInfo) {
        do {
            try core.setWorkingHours(hours: new)
            hours = new
            custom = true
            failure = nil
        } catch {
            failure = "\(error)"
        }
    }

    private func toggle(day: Int) {
        guard var h = hours, h.days.indices.contains(day) else { return }
        h.days[day].toggle()
        save(h)
    }

    /// Minutes after midnight as a time today, for the pickers.
    private func time(_ key: WritableKeyPath<WorkingHoursInfo, UInt32>) -> Binding<Date> {
        Binding(get: {
            let m = Int(hours?[keyPath: key] ?? 0)
            return Calendar.current.date(bySettingHour: m / 60, minute: m % 60, second: 0, of: .now) ?? .now
        }, set: { date in
            guard var h = hours else { return }
            let parts = Calendar.current.dateComponents([.hour, .minute], from: date)
            h[keyPath: key] = UInt32((parts.hour ?? 0) * 60 + (parts.minute ?? 0))
            save(h)
        })
    }

    private func reset() {
        try? core.setWorkingHours(hours: nil)
        load()
    }
}

/// File responses automatically, off until the user turns it on.
struct FileResponsesRow: View {
    let core: Core
    @State private var on = false
    @State private var failure: String?

    var body: some View {
        SettingsRow(title: "File responses automatically",
                    detail: failure ?? "Replies to your meetings that only say yes, no or maybe go to the archive. Replies with a note stay in the inbox.",
                    detailColor: failure == nil ? Color(Theme.text3) : .red) {
            Toggle("File responses automatically", isOn: Binding(get: { on }, set: set))
                .settingsSwitch()
        }
        .onAppear { on = (try? core.fileResponses()) ?? false }
    }

    private func set(_ value: Bool) {
        do {
            try core.setFileResponses(on: value)
            on = value
            failure = nil
        } catch {
            failure = "\(error)"
        }
    }
}

struct CalendarSettingsRow: View {
    let model: AppModel
    let info: CalendarInfo
    @Binding var removal: Confirmation?

    static let swatches = ["#4f6fae", "#3f8a5e", "#8a7a5a", "#9a5a8a", "#c0504d", "#d08a2e", "#2e9aa8", "#7a6fd0"]

    var body: some View {
        let cal = model.calendar
        let core = model.core
        let id = info.id
        HStack(spacing: 12) {
            Menu {
                ForEach(Self.swatches, id: \.self) { hex in
                    Button { change { try core.setCalendarColor(calendarId: id, color: hex) } } label: {
                        Label { Text(hex) } icon: { Image(nsImage: Self.swatch(hex)) }
                    }
                }
                Divider()
                Button("The Server's Color") { change { try core.setCalendarColor(calendarId: id, color: nil) } }
            } label: {
                RoundedRectangle(cornerRadius: 3).fill(Color(CalendarColor.color(info.color, .bar))).frame(width: 12, height: 12)
            }
            .menuStyle(.button)
            .buttonStyle(.plain)
            .menuIndicator(.hidden)
            .fixedSize()
            .help("Color")
            VStack(alignment: .leading, spacing: 2) {
                Text(info.name).font(.system(size: 13, weight: .medium)).lineLimit(1)
                Text(summary).font(.system(size: 12)).foregroundStyle(Color(info.syncError == nil ? Theme.text3 : .systemRed)).lineLimit(2)
            }
            Spacer(minLength: 8)
            if info.kind == .local {
                let accounts = cal.list.accounts.map(\.account)
                let from = accounts.first { $0.id == info.sendFrom }
                Menu {
                    ForEach(accounts, id: \.id) { a in
                        Button(a.address) { change { try core.setSendFrom(calendarId: id, accountId: a.id) } }
                    }
                } label: {
                    Text(from.map { "Invitations from \($0.address)" } ?? "Invitations from…").font(.system(size: 12))
                }
                .menuStyle(.button)
                .buttonStyle(.plain)
                .foregroundStyle(Color(Theme.text2))
                .fixedSize()
                .help("The account Jolta emails this calendar's guests from")
                .accessibilityIdentifier("send-from-menu")
            }
            toggle("Show", info.shown) { on in try core.setCalendarShown(calendarId: id, shown: on) }
            toggle("Busy", info.countsAsBusy) { on in try core.setCalendarBusy(calendarId: id, busy: on) }
                .help("Counts as busy: its events block time")
            toggle("Remind", info.notify) { on in try core.setCalendarNotify(calendarId: id, notify: on) }
                .help("Notifications for this calendar's reminders")
            if info.accountId == nil {
                Button {
                    let message = info.kind == .local
                        ? "Its events are deleted. They exist only on this Mac, so this can't be undone."
                        : "Its events leave Jolta. You can subscribe again later."
                    removal = Confirmation(title: "Remove \(info.name)?", message: message, action: "Remove") {
                        try? core.removeCalendar(calendarId: id)
                        cal.reloadCalendars()
                        cal.refresh()
                    }
                } label: { Image(systemName: "minus.circle") }
                .buttonStyle(.plain)
                .foregroundStyle(Color(Theme.text3))
                .help("Remove this calendar")
            }
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 10)
    }

    private var summary: String {
        if let error = info.syncError { return error }
        var parts = ["\(info.events) events"]
        if let synced = info.synced { parts.append("synced \(Sidebar.wake(synced))") }
        return parts.joined(separator: ", ")
    }

    private func toggle(_ title: String, _ on: Bool, _ set: @escaping (Bool) throws -> Void) -> some View {
        Toggle(title, isOn: Binding(get: { on }, set: { value in change { try set(value) } }))
            .toggleStyle(.checkbox)
            .tint(Color(Theme.accent))
            .font(.system(size: 12))
    }

    private func change(_ action: () throws -> Void) {
        do { try action() } catch { model.notify("\(error)") }
        model.calendar.reloadCalendars()
        model.calendar.refresh()
    }

    static func swatch(_ hex: String) -> NSImage {
        NSImage(size: NSSize(width: 12, height: 12), flipped: false) { rect in
            CalendarColor.color(hex, .bar).setFill()
            NSBezierPath(roundedRect: rect, xRadius: 3, yRadius: 3).fill()
            return true
        }
    }
}

/// The first time calendar mode opens with no calendar on: one sheet to pick them.
struct CalendarIntroSheet: View {
    let model: AppModel
    @State private var switches: CalendarSwitches?

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Which calendars should Jolta show?").font(.system(size: 17, weight: .semibold))
            Text("Calendar is off for every account until you turn it on here. You can change this later in Settings › Calendar.")
                .font(.system(size: 12.5))
                .foregroundStyle(Color(Theme.text2))
                .fixedSize(horizontal: false, vertical: true)
            SettingsCard {
                if let switches { CalendarAccountRows(model: model, switches: switches) }
                SubscribeField(model: model)
            }
            HStack {
                Spacer()
                Button("Done") { model.finishCalendarIntro() }.keyboardShortcut(.defaultAction).tint(Color(Theme.accent))
            }
        }
        .padding(24)
        .frame(width: 560)
        .background(Color(Theme.bg))
        .foregroundStyle(Color(Theme.text))
        .onAppear { if switches == nil { switches = CalendarSwitches(model: model) } }
    }
}

/// The calendar new events go to and how long they last.
struct NewEventDefaults: View {
    let model: AppModel
    @State private var calendar: Int64?
    @State private var minutes = 30
    @State private var failure: String?

    private static let lengths = [15, 30, 45, 60, 90, 120]

    var body: some View {
        let writable = model.calendar.groups.flatMap(\.calendars).map(\.info).filter(\.writable)
        SettingsRow(title: "Default calendar", detail: failure ?? "C and double-click make events here.",
                    detailColor: failure == nil ? Color(Theme.text3) : .red) {
            Picker("Default calendar", selection: Binding(get: { calendar ?? -1 }, set: setCalendar)) {
                if calendar == nil { Text("None").tag(Int64(-1)) }
                ForEach(writable, id: \.id) { c in Text(c.name).tag(c.id) }
            }
            .labelsHidden()
            .fixedSize()
            .disabled(writable.isEmpty)
        }
        SettingsRow(title: "Default length", detail: "How long a new event lasts.") {
            Picker("Default length", selection: Binding(get: { minutes }, set: setLength)) {
                ForEach(Self.lengths, id: \.self) { m in Text(m < 60 ? "\(m) minutes" : m == 60 ? "1 hour" : m == 90 ? "1.5 hours" : "2 hours").tag(m) }
            }
            .labelsHidden()
            .fixedSize()
        }
        .onAppear(perform: load)
    }

    private func load() {
        calendar = (try? model.core.defaultCalendar()) ?? nil
        minutes = Int((try? model.core.defaultEventLength()) ?? 30)
    }

    private func setCalendar(_ id: Int64) {
        guard id >= 0 else { return }
        do {
            try model.core.setDefaultCalendar(calendarId: id)
            calendar = id
            failure = nil
            model.calendar.reloadCalendars()
        } catch {
            failure = "\(error)"
        }
    }

    private func setLength(_ m: Int) {
        do {
            try model.core.setDefaultEventLength(minutes: Int64(m))
            minutes = m
        } catch {
            failure = "\(error)"
        }
    }
}
