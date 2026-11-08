@preconcurrency import JoltaFFI
import AppKit
import SwiftUI

/// The Focus time sheet over the main window, from the palette.
struct FocusSheetRequest: Identifiable {
    let id = UUID()
}

extension AppModel {
    func openFocusSheet() {
        palette = nil
        focusSheet = FocusSheetRequest()
    }
}

extension CalendarModel {
    /// ⌥-drag on empty time: focus time in the calendar new events go to,
    /// declining new invitations on Google, with Undo in the toast.
    func addFocus(start: Int64, end: Int64) {
        do {
            let draft = try core.focusDraft(minutes: max(15, (end - start) / 60))
            let saved = try core.addFocus(calendarId: draft.calendarId, start: start, end: end, declines: draft.declines, doNotDisturb: draft.doNotDisturb)
            refresh()
            toast?(saved.message, saved.token)
        } catch {
            toast?("\(error)", nil)
        }
    }
}

/// Focus time from a start and a length, in a calendar the user can write.
/// The sheet opens at the next free stretch and says what that calendar
/// will get, since Google, Microsoft 365 and the rest each hold it their
/// own way. ⌘Return adds it.
struct FocusSheet: View {
    let model: AppModel
    let close: () -> Void

    static let lengths: [(Int64, String)] = [(30, "30 minutes"), (60, "1 hour"), (90, "1½ hours"), (120, "2 hours"), (180, "3 hours"), (240, "4 hours")]

    @State private var start = Date()
    @State private var minutes: Int64 = 120
    @State private var calendarId: Int64 = 0
    @State private var declines = AwayDeclines.newInvitations
    @State private var doNotDisturb = false
    @State private var plan: FocusPlan?
    @State private var loaded = false
    @State private var working = false
    @State private var failure: String?

    private var calendars: [CalendarInfo] {
        let list = model.calendar.list
        return (list.accounts.flatMap(\.calendars) + list.otherCalendars).filter { $0.writable && $0.shown }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Focus time").font(.system(size: 17, weight: .semibold))
            VStack(alignment: .leading, spacing: 8) {
                field("Starts") {
                    DatePicker("Starts", selection: $start, displayedComponents: [.date, .hourAndMinute])
                        .labelsHidden()
                        .datePickerStyle(.field)
                    Spacer()
                }
                field("For") {
                    Picker("Length", selection: $minutes) {
                        ForEach(Self.lengths, id: \.0) { Text($0.1).tag($0.0) }
                    }
                    .labelsHidden()
                    .fixedSize()
                    .accessibilityIdentifier("focus-length")
                    Text(endText).font(.system(size: 12)).foregroundStyle(Color(Theme.text3))
                    Spacer()
                }
                if !calendars.isEmpty {
                    field("Calendar") {
                        Picker("Calendar", selection: $calendarId) {
                            ForEach(calendars, id: \.id) { c in Text(c.name).tag(c.id) }
                        }
                        .labelsHidden()
                        .fixedSize()
                        Spacer()
                    }
                }
                if plan?.declines == true {
                    field("Decline") {
                        Picker("What Google declines", selection: $declines) {
                            Text("New invitations").tag(AwayDeclines.newInvitations)
                            Text("New invitations and my meetings").tag(AwayDeclines.everything)
                            Text("Nothing").tag(AwayDeclines.nothing)
                        }
                        .labelsHidden()
                        .fixedSize()
                        Spacer()
                    }
                    field("") {
                        Toggle("Do not disturb in Google Chat", isOn: $doNotDisturb).toggleStyle(.checkbox)
                        Spacer()
                    }
                }
            }
            if let line = plan?.line {
                Text(line)
                    .font(.system(size: 12.5))
                    .foregroundStyle(Color(Theme.text2))
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(12)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(RoundedRectangle(cornerRadius: 8).fill(Color(Theme.raise)))
                    .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Color(Theme.line), lineWidth: 1))
                    .accessibilityIdentifier("focus-line")
            }
            if let failure {
                Text(failure).font(.system(size: 12)).foregroundStyle(.red).fixedSize(horizontal: false, vertical: true)
            }
            HStack(spacing: 10) {
                if let problem = plan?.problem {
                    Text(problem).font(.system(size: 12)).foregroundStyle(Color(Theme.text3))
                }
                Spacer()
                Button("Cancel", action: close).keyboardShortcut(.cancelAction)
                Button(action: add) {
                    HStack(spacing: 6) {
                        Text("Add")
                        Text("⌘↩").foregroundStyle(.secondary)
                    }
                }
                .keyboardShortcut(.return, modifiers: .command)
                .tint(Color(Theme.accent))
                .buttonStyle(.borderedProminent)
                .disabled(!canAdd)
                .accessibilityIdentifier("focus-add")
            }
        }
        .padding(22)
        .frame(width: 460)
        .background(Color(Theme.bg))
        .foregroundStyle(Color(Theme.text))
        .accessibilityIdentifier("focus-sheet")
        .onAppear(perform: load)
        .onChange(of: start) { replan() }
        .onChange(of: minutes) { replan() }
        .onChange(of: calendarId) { replan() }
        .onChange(of: declines) { replan() }
        .onChange(of: doNotDisturb) { replan() }
    }

    private var canAdd: Bool { loaded && !working && plan != nil && plan?.problem == nil }

    private static let clock: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "HH:mm"
        return f
    }()

    private var endText: String {
        "until " + Self.clock.string(from: start.addingTimeInterval(TimeInterval(minutes * 60)))
    }

    private func field<Content: View>(_ title: String, @ViewBuilder content: () -> Content) -> some View {
        HStack(spacing: 12) {
            Text(title)
                .font(.system(size: 12.5))
                .foregroundStyle(Color(Theme.text2))
                .frame(width: 64, alignment: .leading)
            content()
        }
    }

    private var current: FocusRequest {
        let from = Int64(start.timeIntervalSince1970)
        return FocusRequest(calendarId: calendarId, start: from, end: from + minutes * 60, declines: declines, doNotDisturb: doNotDisturb)
    }

    private func load() {
        guard !loaded else { return }
        do {
            let draft = try model.core.focusDraft(minutes: minutes)
            start = Date(timeIntervalSince1970: TimeInterval(draft.start))
            calendarId = draft.calendarId
            declines = draft.declines
            doNotDisturb = draft.doNotDisturb
            plan = try model.core.focusPlan(request: draft)
        } catch {
            failure = "\(error)"
        }
        loaded = true
    }

    private func replan() {
        guard loaded else { return }
        do {
            plan = try model.core.focusPlan(request: current)
            failure = nil
        } catch {
            failure = "\(error)"
        }
    }

    private func add() {
        guard canAdd else { return }
        working = true
        let r = current
        do {
            let saved = try model.core.addFocus(calendarId: r.calendarId, start: r.start, end: r.end, declines: r.declines, doNotDisturb: r.doNotDisturb)
            model.calendar.refresh()
            model.notify(saved.message, calendarUndo: saved.token)
            close()
        } catch {
            failure = "\(error)"
        }
        working = false
    }
}
