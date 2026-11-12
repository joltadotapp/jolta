@preconcurrency import JoltaFFI
import AppKit

/// Every single-key command, in one table: the list's key handler, the
/// palette's shortcut column and the help sheet all read from it.
@MainActor
enum Command: String, CaseIterable, Identifiable {
    case next, previous, open, search
    case select, archive, trash, toggleRead, star, snooze, move, label, undo
    case compose, reply, replyAll, forward
    case toggleVIP, unsubscribe, summarize, saveAttachments, nextSection, help
    case accept, maybe, decline, propose, book
    case goInbox, goStarred, goSnoozed, goSent, goDrafts, goArchive

    var id: String { rawValue }

    /// The keys, as typed. Two-key sequences start with g.
    var keys: String {
        switch self {
        case .next: "j"
        case .previous: "k"
        case .open: "↩"
        case .search: "/"
        case .select: "x"
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
        case .saveAttachments: "d"
        case .nextSection: "⇥"
        case .help: "?"
        case .accept: "y"
        case .maybe: "m"
        case .decline: "n"
        case .propose: "p"
        case .book: "b"
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
        case .next: "Next Conversation"
        case .previous: "Previous Conversation"
        case .open: "Open Conversation"
        case .search: "Search"
        case .select: "Select or Deselect Conversation"
        case .archive: "Archive"
        case .trash: "Move to Trash"
        case .toggleRead: "Mark Read or Unread"
        case .star: "Star or Unstar"
        case .snooze: "Snooze…"
        case .move: "Move to…"
        case .label: "Label…"
        case .undo: "Undo"
        case .compose: "New Message"
        case .reply: "Reply"
        case .replyAll: "Reply All"
        case .forward: "Forward"
        case .toggleVIP: "Mark Sender as VIP or Not"
        case .unsubscribe: "Unsubscribe and Archive"
        case .summarize: "Summarize Conversation"
        case .saveAttachments: "Save Attachments…"
        case .nextSection: "Next Inbox Section"
        case .help: "Keyboard Shortcuts"
        case .accept: "Accept Invitation"
        case .maybe: "Answer Invitation Maybe"
        case .decline: "Decline Invitation"
        case .propose: "Propose New Time…"
        case .book: "Book a Held Time…"
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
        case .select: "checkmark.square"
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
        case .saveAttachments: "arrow.down.doc"
        case .nextSection: "rectangle.split.3x1"
        case .help: "keyboard"
        case .accept: "checkmark.circle"
        case .maybe: "questionmark.circle"
        case .decline: "xmark.circle"
        case .propose: "clock.arrow.2.circlepath"
        case .book: "calendar.badge.checkmark"
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
        case .select, .archive, .trash, .toggleRead, .star, .snooze, .move, .label, .reply, .replyAll, .forward, .toggleVIP, .unsubscribe, .summarize, .saveAttachments, .open: true
        case .accept, .maybe, .decline, .propose, .book: true
        default: false
        }
    }

    /// Answers the invitation card in the open conversation.
    var answer: InvitationAnswer? {
        switch self {
        case .accept: .accept
        case .maybe: .maybe
        case .decline: .decline
        default: nil
        }
    }

    /// The Message menu, a divider between the groups.
    static let menu: [[Command]] = [
        [.archive, .trash, .toggleRead, .star, .toggleVIP],
        [.snooze, .move, .label],
        [.reply, .replyAll, .forward],
        [.accept, .maybe, .decline, .propose, .book],
        [.select, .saveAttachments],
        [.undo],
    ]

    /// Its menu item's title. The key is part of the title and not the
    /// item's key equivalent: one without ⌘ can fire while a text field has
    /// the keyboard, and the key router already runs these keys.
    var menuTitle: String { "\(title)  (\(KeyCap.shown(keys)))" }

    static func forKey(_ typed: String, after prefix: String?) -> Command? {
        let sequence = prefix.map { "\($0) \(typed)" } ?? typed
        return allCases.first { $0.keys == sequence }
    }
}

extension AppModel {
    /// Whether the Message menu offers the command now: in mail, with a
    /// conversation under the cursor when it acts on one, and not while a
    /// message is being written. Undo also takes a calendar change back.
    func offers(_ command: Command) -> Bool {
        if command == .undo { return compose == nil }
        return mode == .mail && compose == nil && (!command.needsThread || cursorRow != nil)
    }

    /// Edit › Undo: the typing in the text field that has the keyboard,
    /// else the last mail or calendar action.
    func undoFromMenu() {
        guard let window = NSApp.keyWindow else { return }
        if let text = window.firstResponder as? NSTextView, text.isEditable, let typing = text.undoManager, typing.canUndo {
            typing.undo()
        } else if window === KeyRouter.shared.window {
            undo()
        }
    }

    func redoFromMenu() {
        if let text = NSApp.keyWindow?.firstResponder as? NSTextView, let typing = text.undoManager, typing.canRedo { typing.redo() }
    }

    func run(_ command: Command) {
        let row = cursorRow
        switch command {
        case .next, .previous, .open, .nextSection:
            break
        case .search: requestSearchFocus()
        case .select: togglePicked()
        case .archive: act(.archive, label: "Archived")
        case .trash: act(.trash, label: "Moved to Trash")
        case .toggleRead:
            if (row?.unreadCount ?? 0) > 0 { act(.markRead, label: "Marked read") } else { act(.markUnread, label: "Marked unread") }
        case .star:
            if row?.flagged == true { act(.unflag, label: "Unstarred") } else { act(.flag, label: "Starred") }
        case .snooze: palette = .snooze
        case .move: palette = .move
        case .label: openLabels()
        case .undo: undo()
        case .compose: startCompose()
        case .reply: reply(.reply, threadID: row?.threadId)
        case .replyAll where shownInvitation == nil && shownProposal?.accepting == true:
            if let p = shownProposal { acceptProposal(message: p.messageId) }
        case .replyAll: reply(.replyAll, threadID: row?.threadId)
        case .forward: reply(.forward, threadID: row?.threadId)
        case .toggleVIP: toggleVIP()
        case .unsubscribe: unsubscribe()
        case .summarize: if searchFocused { searchWithAI() } else { summarizeOpenThread() }
        case .saveAttachments:
            if let message = openThread?.filesMessageId { saveAttachments(message: message) } else { notify("No attachments in this conversation") }
        case .help: showsHelp.toggle()
        case .decline where shownInvitation == nil && shownProposal?.declining == true:
            if let p = shownProposal { declineProposal(message: p.messageId) }
        case .accept, .maybe, .decline: if let answer = command.answer { answerInvitation(answer) }
        case .propose: openPropose()
        case .book: bookHold()
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
    case today, next, previous, day, workWeek, week, month, agenda, toggleCalendar, move, open, close, newEvent, quickCreate, goToDate, edit, duplicate, delete
    case nudge, resize, accept, maybe, decline, undo, dayPanel, mail, calendar, joinNext

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
        case .newEvent: "c"
        case .quickCreate: "q"
        case .goToDate: "g"
        case .edit: "e"
        case .duplicate: "⌘D"
        case .delete: "⌫"
        case .nudge: "⌥↑↓"
        case .resize: "⇧↑↓"
        case .accept: "y"
        case .maybe: "⇧M"
        case .decline: "n"
        case .undo: "z"
        case .dayPanel: "⇧C"
        case .mail: "⌘1"
        case .calendar: "⌘2"
        case .joinNext: "⇧⌘J"
        }
    }

    var title: String {
        switch self {
        case .today: "Today"
        case .next: "Next Period"
        case .previous: "Previous Period"
        case .day: "Day"
        case .workWeek: "Work Week"
        case .week: "Week"
        case .month: "Month"
        case .agenda: "Agenda"
        case .toggleCalendar: "Show or Hide Calendar 1 to 9"
        case .move: "Move Between Events"
        case .open: "Open Event"
        case .close: "Close Event"
        case .newEvent: "New Event"
        case .quickCreate: "Quick Create: Type the Event"
        case .goToDate: "Go to Date"
        case .edit: "Edit Event"
        case .duplicate: "Duplicate Event"
        case .delete: "Delete Event"
        case .nudge: "Move Event 15 Minutes"
        case .resize: "Shorten or Lengthen by 15 Minutes"
        case .accept: "Accept Invitation"
        case .maybe: "Maybe"
        case .decline: "Decline Invitation"
        case .undo: "Undo"
        case .dayPanel: "Today Beside Mail"
        case .mail: "Mail"
        case .calendar: "Calendar"
        case .joinNext: "Join Next Meeting"
        }
    }

    /// The command a key typed in calendar mode runs.
    static func forKey(_ event: NSEvent) -> CalendarCommand? {
        switch event.keyCode {
        case 125 where event.modifierFlags.contains(.option), 126 where event.modifierFlags.contains(.option): return .nudge
        case 125 where event.modifierFlags.contains(.shift), 126 where event.modifierFlags.contains(.shift): return .resize
        case 123, 124, 125, 126: return .move
        case 36, 76: return .open
        case 53: return .close
        case 51, 117: return .delete
        default: break
        }
        if digit(event) != nil { return .toggleCalendar }
        guard let typed = event.charactersIgnoringModifiers?.lowercased(), typed.count == 1 else { return nil }
        if typed == "c", event.modifierFlags.contains(.shift) { return .dayPanel }
        // M is Month; Maybe takes ⇧M.
        if typed == "m", event.modifierFlags.contains(.shift) { return .maybe }
        return allCases.first { $0.keys == typed }
    }

    /// 1 to 9 by the key's place on the top row, so AZERTY's unshifted & é " work too.
    static func digit(_ event: NSEvent) -> Int? {
        [18: 1, 19: 2, 20: 3, 21: 4, 23: 5, 22: 6, 26: 7, 28: 8, 25: 9][event.keyCode]
    }
}
