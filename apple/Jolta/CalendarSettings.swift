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

    func set(_ account: CalendarAccount, on: Bool) {
        let id = account.account.id
        messages[id] = nil
        let core = model.core
        working[id] = account.needsConsent && on ? "Continue in your browser…" : on ? "Finding calendars…" : "Turning off…"
        Task {
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

/// Settings › Calendar: accounts' switches, each calendar's settings, subscriptions.
struct CalendarSettings: View {
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
                SettingsSection(group.title, footer: group.id == "feeds" ? "Subscriptions are read-only." : nil) {
                    ForEach(group.calendars) { entry in CalendarSettingsRow(model: model, info: entry.info, removal: $removal) }
                }
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

struct CalendarSettingsRow: View {
    let model: AppModel
    let info: CalendarInfo
    @Binding var removal: Confirmation?

    private static let swatches = ["#4f6fae", "#3f8a5e", "#8a7a5a", "#9a5a8a", "#c0504d", "#d08a2e", "#2e9aa8", "#7a6fd0"]

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
            toggle("Show", info.shown) { on in try core.setCalendarShown(calendarId: id, shown: on) }
            toggle("Busy", info.countsAsBusy) { on in try core.setCalendarBusy(calendarId: id, busy: on) }
                .help("Counts as busy: its events block time")
            toggle("Remind", info.notify) { on in try core.setCalendarNotify(calendarId: id, notify: on) }
                .help("Notifications for this calendar's reminders")
            if info.accountId == nil {
                Button {
                    removal = Confirmation(title: "Remove \(info.name)?", message: "Its events leave Jolta. You can subscribe again later.", action: "Remove") {
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
