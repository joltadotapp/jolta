@preconcurrency import JoltaFFI
import Foundation

/// An event changed on the grid and not saved yet: dropped and waiting for
/// the scope prompt or Send an update?, or moved with keys in the last second.
struct GridChange {
    /// The occurrence as it is stored.
    let event: CalEvent
    /// The event at its new time; for a copy, the new event.
    var draft: EventDraft
    /// ⌥-drag: a new event, and the original stays.
    let copy: Bool

    /// The event where the grid draws it until it is saved.
    var preview: CalEvent {
        var record = event.record
        record.time = draft.time
        return CalEvent(record)
    }
}

/// Moving, resizing and copying events with the mouse and keys. Every
/// change saves the way the editor's Save does: the scope prompt for an
/// occurrence of a series, Send an update? for a meeting with guests, then
/// the op, which Undo takes back.
extension CalendarModel {
    /// Whether a drag may change the item's time: not for a meeting someone
    /// else organizes or an event in a read-only calendar.
    func canMove(_ item: GridTarget) -> Bool {
        switch item {
        case .draft: editor.map { !$0.locked(.time) } ?? false
        case .event(let event):
            (try? core.draftForEvent(eventId: event.record.eventId, time: event.record.time))?.locks.contains(.time) == false
        }
    }

    /// The period's events as the month draws them: a moved event at its new time.
    var shownEvents: [CalEvent] {
        guard let change = gridChange else { return events }
        return (change.copy ? events : events.filter { $0.id != change.event.id }) + [change.preview]
    }

    /// A drag let go on the hour grid.
    func drop(_ drop: GridDrop) {
        let index: Int
        switch drop {
        case .create(let d, _, _), .focus(let d, _, _), .move(_, let d, _, _), .resize(_, _, let d, _): index = d
        }
        guard days.indices.contains(index) else { return }
        let day = days[index]
        let at = { (minute: Int) in Int64(Self.at(day.start, minutesOn: minute).timeIntervalSince1970) }
        switch drop {
        case .create(_, let top, let bottom):
            newEvent(at: .timed(start: at(top), end: at(bottom)))
        case .focus(_, let top, let bottom):
            addFocus(start: at(top), end: at(bottom))
        case .move(let item, _, let top, let copy):
            reschedule(item, copy: copy) { start, end in (at(top), at(top) + end - start) }
        case .resize(let item, let isTop, _, let minute):
            reschedule(item, copy: false) { start, end in
                let new = at(minute)
                return isTop ? (new < end ? (new, end) : nil) : (new > start ? (start, new) : nil)
            }
        }
    }

    /// The editor's draft follows the drag; an event saves at its new time.
    private func reschedule(_ item: GridTarget, copy: Bool, _ times: (Int64, Int64) -> (Int64, Int64)?) {
        switch item {
        case .draft:
            guard let editor, !editor.locked(.time), case .timed(let s, let e) = editor.draft.time, let (start, end) = times(s, e) else { return }
            editor.draft.time = .timed(start: start, end: end)
            editor.refreshTexts()
        case .event(let event):
            guard case .timed(let s, let e) = event.record.time, let (start, end) = times(s, e) else { return }
            change(event, to: .timed(start: start, end: end), copy: copy)
        }
    }

    /// An event dragged in the all-day row or the month, `days` days on.
    func move(_ event: CalEvent, days: Int, copy: Bool) {
        guard days != 0 else { return }
        let shift = { (date: Date) in Self.calendar.date(byAdding: .day, value: days, to: date) ?? date }
        switch event.record.time {
        case .allDay(let first, let last):
            guard let a = Self.date(key: first), let b = Self.date(key: last) else { return }
            change(event, to: .allDay(firstDay: Self.dayKey(shift(a)), lastDay: Self.dayKey(shift(b))), copy: copy)
        case .timed(let s, let e):
            let start = Int64(shift(Date(timeIntervalSince1970: TimeInterval(s))).timeIntervalSince1970)
            change(event, to: .timed(start: start, end: start + e - s), copy: copy)
        }
    }

    /// ⌥↑ and ⌥↓ move the selected event 15 minutes; ⇧↑ and ⇧↓ make it 15
    /// minutes shorter or longer. Presses less than a second apart add up
    /// to one save, so the guests get one update.
    func nudge(moving: Bool, by minutes: Int) {
        guard let event = selected, event.isTimed, !event.isLong else { return }
        if let pending = gridChange {
            // A dialog is open for an earlier change.
            guard gathering != nil else { return }
            if pending.event.id != event.id {
                commitChange()
                guard gridChange == nil else { return }
            }
        }
        guard var change = gridChange ?? newChange(event, copy: false),
              case .timed(let s, let e) = change.draft.time else { return }
        let d = Int64(minutes * 60)
        if moving {
            change.draft.time = .timed(start: s + d, end: e + d)
        } else {
            guard e + d - s >= 15 * 60 else { return }
            change.draft.time = .timed(start: s, end: e + d)
        }
        gridChange = change
        updateGrids()
        gathering?.cancel()
        gathering = Task { [weak self] in
            try? await Task.sleep(for: .seconds(1))
            guard !Task.isCancelled else { return }
            self?.commitChange()
        }
    }

    /// Saves keys still adding up now, before another action reads the event.
    func settleKeys() {
        if gathering != nil { commitChange() }
    }

    /// The draft for moving or copying `event`; nil, with the reason in a
    /// toast, when its time can't change.
    private func newChange(_ event: CalEvent, copy: Bool) -> GridChange? {
        do {
            if copy { return GridChange(event: event, draft: try copyDraft(of: event), copy: true) }
            let draft = try core.draftForEvent(eventId: event.record.eventId, time: event.record.time)
            guard !draft.locks.contains(.time) else {
                toast?(draft.notice ?? "\(event.title) can't be moved.", nil)
                return nil
            }
            return GridChange(event: event, draft: draft, copy: false)
        } catch {
            toast?("\(error)", nil)
            return nil
        }
    }

    private func change(_ event: CalEvent, to time: OccurrenceTime, copy: Bool) {
        settleKeys()
        guard gridChange == nil, var change = newChange(event, copy: copy) else { return }
        change.draft.time = time
        gridChange = change
        updateGrids()
        commitChange()
    }

    /// Saves the change through the editor's questions. Cancel puts the
    /// event back; Keep editing opens the editor at the new time.
    func commitChange() {
        gathering?.cancel()
        gathering = nil
        guard let change = gridChange else { return }
        let failed = { [weak self] (text: String) in
            self?.dropChange()
            self?.toast?(text, nil)
        }
        do {
            if try core.planSave(draft: change.draft).unchanged { return dropChange() }
            try confirmSave(change.draft, cancel: { [weak self] in self?.dropChange() },
                            back: { [weak self] in self?.keepEditing(change) }, failed: failed) { [weak self] scope, notify, message in
                self?.save(change.draft, scope: scope, notify: notify, message: message, failed: failed) { self?.gridChange = nil }
            }
        } catch {
            failed("\(error)")
        }
    }

    private func dropChange() {
        gathering?.cancel()
        gathering = nil
        gridChange = nil
        updateGrids()
    }

    private func keepEditing(_ change: GridChange) {
        dropChange()
        guard confirmLeaving({ [weak self] in self?.keepEditing(change) }) else { return }
        open(EventEditorModel(core: core, draft: change.draft, event: change.copy ? nil : change.event))
    }
}
