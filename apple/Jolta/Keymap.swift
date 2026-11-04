import AppKit

/// Every single-key command, in one table: the list's key handler, the
/// palette's shortcut column and the help sheet all read from it.
@MainActor
enum Command: String, CaseIterable, Identifiable {
    case next, previous, open, search
    case archive, trash, toggleRead, star, snooze, move, label, undo
    case compose, reply, replyAll, forward
    case toggleVIP, unsubscribe, summarize, nextSection, help
    case goInbox, goStarred, goSnoozed, goSent, goDrafts, goArchive

    var id: String { rawValue }

    /// The keys, as typed. Two-key sequences start with g.
    var keys: String {
        switch self {
        case .next: "j"
        case .previous: "k"
        case .open: "↩"
        case .search: "/"
        case .archive: "e"
        case .trash: "#"
        case .toggleRead: "u"
        case .star: "s"
        case .snooze: "h"
        case .move: "v"
        case .label: "l"
        case .undo: "z"
        case .compose: "c"
        case .reply: "r"
        case .replyAll: "a"
        case .forward: "f"
        case .toggleVIP: "!"
        case .unsubscribe: "⌘U"
        case .summarize: "⌘J"
        case .nextSection: "⇥"
        case .help: "?"
        case .goInbox: "g i"
        case .goStarred: "g s"
        case .goSnoozed: "g h"
        case .goSent: "g t"
        case .goDrafts: "g d"
        case .goArchive: "g e"
        }
    }

    var title: String {
        switch self {
        case .next: "Next conversation"
        case .previous: "Previous conversation"
        case .open: "Open conversation"
        case .search: "Search"
        case .archive: "Archive"
        case .trash: "Move to Trash"
        case .toggleRead: "Mark read or unread"
        case .star: "Star or unstar"
        case .snooze: "Snooze…"
        case .move: "Move to…"
        case .label: "Label…"
        case .undo: "Undo"
        case .compose: "New message"
        case .reply: "Reply"
        case .replyAll: "Reply all"
        case .forward: "Forward"
        case .toggleVIP: "Mark sender as VIP or not"
        case .unsubscribe: "Unsubscribe and archive"
        case .summarize: "Summarize conversation"
        case .nextSection: "Next inbox section"
        case .help: "Keyboard shortcuts"
        case .goInbox: "Go to Inbox"
        case .goStarred: "Go to Starred"
        case .goSnoozed: "Go to Snoozed"
        case .goSent: "Go to Sent"
        case .goDrafts: "Go to Drafts"
        case .goArchive: "Go to Archive"
        }
    }

    var icon: String {
        switch self {
        case .next: "arrow.down"
        case .previous: "arrow.up"
        case .open: "envelope.open"
        case .search: "magnifyingglass"
        case .archive: "archivebox"
        case .trash: "trash"
        case .toggleRead: "envelope.badge"
        case .star: "star"
        case .snooze: "clock"
        case .move: "folder"
        case .label: "tag"
        case .undo: "arrow.uturn.backward"
        case .compose: "square.and.pencil"
        case .reply: "arrowshape.turn.up.left"
        case .replyAll: "arrowshape.turn.up.left.2"
        case .forward: "arrowshape.turn.up.right"
        case .toggleVIP: "checkmark.seal"
        case .unsubscribe: "envelope.badge.minus"
        case .summarize: "sparkles"
        case .nextSection: "rectangle.split.3x1"
        case .help: "keyboard"
        case .goInbox: "tray"
        case .goStarred: "star"
        case .goSnoozed: "clock"
        case .goSent: "paperplane"
        case .goDrafts: "pencil"
        case .goArchive: "archivebox"
        }
    }

    /// Whether the command acts on the conversation under the cursor.
    var needsThread: Bool {
        switch self {
        case .archive, .trash, .toggleRead, .star, .snooze, .move, .label, .reply, .replyAll, .forward, .toggleVIP, .unsubscribe, .summarize, .open: true
        default: false
        }
    }

    static func forKey(_ typed: String, after prefix: String?) -> Command? {
        let sequence = prefix.map { "\($0) \(typed)" } ?? typed
        return allCases.first { $0.keys == sequence }
    }
}

extension AppModel {
    func run(_ command: Command) {
        let row = cursorRow
        switch command {
        case .next, .previous, .open, .nextSection:
            break
        case .search: requestSearchFocus()
        case .archive: act(.archive, label: "Archived")
        case .trash: act(.trash, label: "Moved to Trash")
        case .toggleRead:
            if (row?.unreadCount ?? 0) > 0 { act(.markRead, label: "Marked read") } else { act(.markUnread, label: "Marked unread") }
        case .star:
            if row?.flagged == true { act(.unflag, label: "Unstarred") } else { act(.flag, label: "Starred") }
        case .snooze: palette = .snooze
        case .move, .label: palette = .move
        case .undo: undo()
        case .compose: startCompose()
        case .reply: reply(.reply, threadID: row?.threadId)
        case .replyAll: reply(.replyAll, threadID: row?.threadId)
        case .forward: reply(.forward, threadID: row?.threadId)
        case .toggleVIP: toggleVIP()
        case .unsubscribe: unsubscribe()
        case .summarize: if searchFocused { searchWithAI() } else { summarizeOpenThread() }
        case .help: showsHelp.toggle()
        case .goInbox: source = .unifiedInbox
        case .goStarred: source = .starred
        case .goSnoozed: source = .role("snoozed")
        case .goSent: source = .role("sent")
        case .goDrafts: source = .role("drafts")
        case .goArchive: source = .role("archive")
        }
    }
}

/// Calendar mode's keys, in one table for the key handler and the help sheet.
enum CalendarCommand: String, CaseIterable, Identifiable {
    case today, next, previous, day, workWeek, week, month, agenda, toggleCalendar, move, open, close, dayPanel, mail, calendar, joinNext

    var id: String { rawValue }

    var keys: String {
        switch self {
        case .today: "t"
        case .next: "j"
        case .previous: "k"
        case .day: "d"
        case .workWeek: "x"
        case .week: "w"
        case .month: "m"
        case .agenda: "a"
        case .toggleCalendar: "1–9"
        case .move: "↑↓←→"
        case .open: "↩"
        case .close: "esc"
        case .dayPanel: "⇧C"
        case .mail: "⌘1"
        case .calendar: "⌘2"
        case .joinNext: "⇧⌘J"
        }
    }

    var title: String {
        switch self {
        case .today: "Today"
        case .next: "Next period"
        case .previous: "Previous period"
        case .day: "Day"
        case .workWeek: "Work week"
        case .week: "Week"
        case .month: "Month"
        case .agenda: "Agenda"
        case .toggleCalendar: "Show or hide calendar 1 to 9"
        case .move: "Move between events"
        case .open: "Open event"
        case .close: "Close event"
        case .dayPanel: "Today beside mail"
        case .mail: "Mail"
        case .calendar: "Calendar"
        case .joinNext: "Join next meeting"
        }
    }

    /// The command a key typed in calendar mode runs.
    static func forKey(_ event: NSEvent) -> CalendarCommand? {
        switch event.keyCode {
        case 123, 124, 125, 126: return .move
        case 36, 76: return .open
        case 53: return .close
        default: break
        }
        if digit(event) != nil { return .toggleCalendar }
        guard let typed = event.charactersIgnoringModifiers?.lowercased(), typed.count == 1 else { return nil }
        if typed == "c", event.modifierFlags.contains(.shift) { return .dayPanel }
        return allCases.first { $0.keys == typed }
    }

    /// 1 to 9 by the key's place on the top row, so AZERTY's unshifted & é " work too.
    static func digit(_ event: NSEvent) -> Int? {
        [18: 1, 19: 2, 20: 3, 21: 4, 23: 5, 22: 6, 26: 7, 28: 8, 25: 9][event.keyCode]
    }
}
