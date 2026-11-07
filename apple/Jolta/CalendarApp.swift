@preconcurrency import JoltaFFI
import AppKit

/// The app's side of the calendar: switching modes, keys, reminders and joining.
extension AppModel {
    func startCalendar() {
        calendar.onChange = { [weak self] in self?.scheduleReminders() }
        calendar.toast = { [weak self] text, token in self?.notify(text, calendarUndo: token) }
        notifier.onOpenEvent = { [weak self] id, fireAt in
            guard let self else { return }
            showMainWindow()
            mode = .calendar
            guard let r = reminder(id, fireAt) else { return notify("That event changed or is no longer in Jolta") }
            calendar.reveal(eventID: r.eventId, time: r.time)
        }
        notifier.onJoin = { [weak self] id, fireAt in
            guard let self, let r = reminder(id, fireAt) else { return }
            join(eventID: r.eventId, time: r.time)
        }
        KeyRouter.shared.calendarHandler = { [weak self] event in self?.handleCalendarKey(event) ?? false }
        scheduleReminders()
        // The 48 hours ahead move on; pick up reminders that came into range.
        Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(3600))
                self?.scheduleReminders()
            }
        }
        // Launch arguments for checking the calendar from scripts:
        // `-JoltaMode calendar -JoltaSpan month -JoltaOpenEvent "Design review" -JoltaDayPanel YES`.
        let defaults = UserDefaults.standard
        if let span = defaults.string(forKey: "JoltaSpan").flatMap(CalendarSpan.init(rawValue:)) { calendar.span = span }
        if defaults.string(forKey: "JoltaDayPanel") != nil { showsDayPanel = defaults.bool(forKey: "JoltaDayPanel") }
        if defaults.string(forKey: "JoltaMode") == "calendar" { mode = .calendar }
        if let title = defaults.string(forKey: "JoltaOpenEvent"), let event = calendar.events.first(where: { $0.title == title }) {
            calendar.openDetail(event)
        }
        if let bench = CalendarBench.shared {
            Task {
                try? await Task.sleep(for: .seconds(3))
                if let window = KeyRouter.shared.window { bench.run(window: window) { [weak self] in self?.mode = .calendar } }
            }
        }
    }

    func modeChanged() {
        palette = nil
        showsHelp = false
        if mode == .calendar {
            calendar.reloadCalendars()
            calendar.refresh()
            if calendar.hasNoCalendars, !UserDefaults.standard.bool(forKey: Self.key("CalendarIntroDone")) { showsCalendarIntro = true }
        } else {
            calendar.closeDetail()
            if compose == nil { requestListFocus() }
        }
    }

    func finishCalendarIntro() {
        UserDefaults.standard.set(true, forKey: Self.key("CalendarIntroDone"))
        showsCalendarIntro = false
    }

    /// Reschedules the next 48 hours of reminders a moment after the last
    /// change, so a sync that touches many calendars reschedules once.
    func scheduleReminders() {
        reminderTask?.cancel()
        let core = core, notifier = notifier
        reminderTask = Task {
            try? await Task.sleep(for: .seconds(1))
            guard !Task.isCancelled else { return }
            let now = Int64(Date.now.timeIntervalSince1970)
            guard let reminders = try? core.calendarReminders(from: now, to: now + 48 * 3600) else { return }
            await notifier.schedule(reminders: reminders) { id, fireAt in (try? core.calendarReminder(id: id, fireAt: fireAt)) != nil }
        }
    }

    /// The palette's and ⇧⌘J's Join next meeting.
    func joinNextMeeting() {
        calendar.refreshNextMeeting()
        guard let next = calendar.nextMeeting else { return notify("No online meeting in the next week") }
        CalendarModel.join(next.meeting)
        notify("Joining \(next.title)")
    }

    /// A `webcal://` link clicked anywhere: asks, then subscribes and shows the calendar.
    func subscribe(link url: URL) {
        guard ["webcal", "webcals"].contains(url.scheme?.lowercased() ?? "") else { return }
        let alert = NSAlert()
        alert.messageText = "Subscribe to this calendar?"
        alert.informativeText = url.absoluteString
        alert.addButton(withTitle: "Subscribe")
        alert.addButton(withTitle: "Cancel")
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        notify("Subscribing…")
        Task {
            do {
                let info = try await core.subscribeCalendar(url: url.absoluteString)
                notify(info.syncError.map { "Subscribed to \(info.name), but the download failed: \($0)" } ?? "Subscribed to \(info.name)")
                calendar.reloadCalendars()
                calendar.refresh()
                mode = .calendar
            } catch {
                notify("Couldn't subscribe: \(error)")
            }
        }
    }

    /// A delivered or snoozed reminder as it stands now: syncs replace the
    /// event rows it was scheduled with.
    private func reminder(_ id: String, _ fireAt: Int64) -> ReminderRecord? {
        (try? core.calendarReminder(id: id, fireAt: fireAt)) ?? nil
    }

    private func join(eventID: Int64, time: OccurrenceTime?) {
        if let meeting = (try? core.calendarEvent(eventId: eventID, time: time, appCss: ""))?.meeting {
            CalendarModel.join(meeting)
        }
    }

    private func showMainWindow() {
        NSApp.activate()
        KeyRouter.shared.window?.makeKeyAndOrderFront(nil)
    }

    /// Calendar mode's keys, and ⇧C in mail. Returns false to leave a key to mail.
    func handleCalendarKey(_ event: NSEvent) -> Bool {
        CalendarBench.shared?.mark("key received")
        if event.modifierFlags.contains(.command) { return false }
        let command = CalendarCommand.forKey(event)
        if mode == .mail {
            guard command == .dayPanel, palette == nil, !showsHelp, compose == nil else { return false }
            showsDayPanel.toggle()
            return true
        }
        if palette != nil || showsHelp {
            if event.keyCode == 53 { palette = nil; showsHelp = false }
            return true
        }
        let cal = calendar
        // The editor's fields and buttons take the keys; Esc closes it.
        if cal.editor != nil || cal.discard != nil {
            guard event.keyCode == 53, cal.discard == nil else { return false }
            cal.closeEditor()
            return true
        }
        if event.characters == "?" { showsHelp = true; return true }
        switch command {
        case .today: cal.goToday()
        case .next: cal.shift(1); CalendarBench.shared?.mark("key handled")
        case .previous: cal.shift(-1); CalendarBench.shared?.mark("key handled")
        case .day: cal.span = .day
        case .workWeek: cal.span = .workWeek
        case .week: cal.span = .week
        case .month: cal.span = .month
        case .agenda: cal.span = .agenda
        case .toggleCalendar: if let n = CalendarCommand.digit(event) { cal.toggleCalendar(number: n) }
        case .move:
            let move: CalendarModel.SelectionMove = switch event.keyCode {
            case 123: .left
            case 124: .right
            case 125: .down
            default: .up
            }
            cal.moveSelection(move)
        case .newEvent: cal.newEvent()
        case .edit: cal.editSelected()
        case .delete: cal.deleteSelected()
        case .undo: undo()
        case .open: cal.openSelected()
        case .close: if cal.detail != nil { cal.closeDetail() } else { cal.selectedID = nil }
        case .dayPanel, .mail, .calendar, .joinNext, .duplicate, nil: break
        }
        return true
    }
}
