@preconcurrency import JoltaFFI
import SwiftUI

enum PaletteMode: Equatable {
    case commands
    case snooze
    /// When the message being written should go out.
    case sendLater
    /// When to bring the message being written back if nobody answers.
    case remind
    /// A snippet to insert at the cursor of the message being written.
    case snippets
    case move
    /// Gmail labels to add to or take off the conversation under the cursor.
    case label
    /// A theme to show, previewed as the selection moves.
    case themes
    /// The kind of account to add.
    case accounts
    /// Q and New event: a line read into an event (`QuickCreateView`).
    case quickEvent
    /// G: a typed day to show in the calendar (`QuickCreateView`).
    case goToDate
}

private struct PaletteItem: Identifiable {
    let id: String
    let title: String
    var detail: String = ""
    let icon: String
    var keys: String?
    /// A theme row shows the theme's colors and previews it while selected.
    var theme: ThemePalette?
    /// Runs with the palette left open, for items that switch it to another list.
    var keepsOpen = false
    /// A label the conversation already has.
    var checked = false
    let run: () -> Void
}

/// ⌘K: every command, mailbox and section by name. The same panel picks a
/// snooze time (H), a destination folder (V), a Gmail label (L) and a theme.
struct PaletteView: View {
    @Bindable var model: AppModel
    let mode: PaletteMode
    @State private var text = ""
    @State private var selected = 0
    /// Where the pointer last selected a row, in screen coordinates. Rows that
    /// scroll under a pointer that hasn't moved don't take the selection.
    @State private var pointer: CGPoint?
    /// The pointer chose the selection, so the list doesn't scroll to it.
    @State private var pointerSelected = false
    @FocusState private var focused: Bool

    private var placeholder: String {
        switch mode {
        case .commands: "Type a command, mailbox or section"
        case .snooze: "Snooze until… tomorrow 9am, mon, in 2h"
        case .sendLater: "Send at… tomorrow 9am, mon 8:30, in 2h"
        case .remind: "Remind me if no reply… in 3 days, fri, next week"
        case .snippets: "Insert a snippet"
        case .move: "Move to folder"
        case .label: "Add or remove a label"
        case .themes: "Choose a theme"
        case .accounts: "Add an account"
        case .quickEvent, .goToDate: ""
        }
    }

    private var icon: String {
        switch mode {
        case .snooze: "clock"
        case .sendLater: "paperplane"
        case .remind: "bell"
        case .snippets: "text.badge.plus"
        case .themes: "paintpalette"
        case .label: "tag"
        case .accounts: "person.crop.circle.badge.plus"
        case .commands, .move, .quickEvent, .goToDate: "magnifyingglass"
        }
    }

    private var items: [PaletteItem] {
        let all: [PaletteItem]
        switch mode {
        case .commands: all = commandItems
        case .snooze, .sendLater, .remind: return timeItems
        case .move: all = moveItems
        case .label: all = labelItems
        case .snippets: all = snippetItems
        case .themes: all = themeItems
        case .accounts: all = accountItems
        case .quickEvent, .goToDate: return []
        }
        // "frappe" finds Catppuccin Frappé, "geneve" a folder called Genève.
        let fold = { (s: String) in s.folding(options: [.caseInsensitive, .diacriticInsensitive], locale: nil) }
        let needle = fold(text).trimmingCharacters(in: .whitespaces)
        guard !needle.isEmpty else { return all }
        let scored = all.compactMap { item -> (Int, PaletteItem)? in
            guard let s = score(fold(item.title), needle) else { return nil }
            return (s, item)
        }
        var out = scored.sorted { $0.0 > $1.0 }.map(\.1)
        if mode == .commands {
            out.append(PaletteItem(id: "search", title: "Search for “\(text)”", icon: "magnifyingglass") { [model, text] in
                model.source = .allMail
                model.search = text
            })
        }
        return out
    }

    private var commandItems: [PaletteItem] {
        let hasThread = model.cursorRow != nil
        var out: [PaletteItem] = Command.allCases
            .filter { ![.next, .previous, .open, .help].contains($0) && (!$0.needsThread || hasThread) }
            .filter { c in
                switch c {
                case .propose: model.shownInvitation?.newTime != nil
                case .book: model.holdLine?.slots.contains { $0.state == .held } == true
                case .decline: model.shownInvitation != nil || model.shownProposal != nil
                case .label: model.cursorHasLabels
                default: c.answer == nil || model.shownInvitation != nil
                }
            }
            .map { c in PaletteItem(id: c.rawValue, title: c.title, icon: c.icon, keys: c.keys) { [model] in model.run(c) } }
        for s in model.sections {
            out.append(PaletteItem(id: "section-\(s.id)", title: "Show \(s.name)", detail: s.query, icon: "rectangle.split.3x1") { [model] in
                model.source = .unifiedInbox
                model.section = s
            })
        }
        for account in model.accounts {
            for box in account.mailboxes {
                out.append(PaletteItem(id: "box-\(box.id)", title: "Go to \(box.name)", detail: account.title, icon: "folder") { [model] in
                    model.source = .mailbox(id: box.id, name: box.name)
                })
            }
        }
        out.append(PaletteItem(id: "mark-all-read", title: "Mark All as Read", detail: model.source.title, icon: "envelope.open") { [model] in
            model.markAllRead()
        })
        if !model.trashFolders(model.source).isEmpty {
            out.append(PaletteItem(id: "empty-trash", title: "Empty Trash…", icon: "trash.slash") { [model] in
                model.confirmEmpty = model.source
            })
        }
        if model.compose != nil {
            out.append(PaletteItem(id: "insert-availability", title: "Insert My Availability", detail: "Free times in the next working days",
                                   icon: "calendar.badge.clock") { [model] in model.insertMyAvailability() })
        } else if model.selectedThreadID != nil {
            out.append(PaletteItem(id: "find-time", title: "Find a Time", detail: "With the people on this conversation",
                                   icon: "calendar.badge.clock") { [model] in model.findTimeForConversation() })
        }
        out.append(PaletteItem(id: "new-event", title: "New Event", detail: "Type it: Lunch with Jan fri 12:30 at Mok", icon: "calendar.badge.plus",
                               keys: model.mode == .calendar ? CalendarCommand.quickCreate.keys : nil, keepsOpen: true) { [model] in
            model.palette = .quickEvent
        })
        if model.mode == .calendar {
            out.append(PaletteItem(id: "go-to-date", title: "Go to Date", detail: "fri, 14 dec, morgen", icon: "calendar",
                                   keys: CalendarCommand.goToDate.keys, keepsOpen: true) { [model] in model.palette = .goToDate })
        }
        out.append(PaletteItem(id: "join-next", title: "Join Next Meeting", detail: model.calendar.nextMeeting?.title ?? "",
                               icon: "video", keys: CalendarCommand.joinNext.keys) { [model] in model.joinNextMeeting() })
        let emailing = model.mode == .calendar ? model.calendar.selected?.title : nil
        out.append(PaletteItem(id: "email-guests", title: "Email Guests", detail: emailing ?? "The meeting now or next, with other people",
                               icon: "envelope") { [model] in model.emailGuestsOfNextMeeting() })
        if model.mode == .mail {
            out.append(PaletteItem(id: "calendar", title: "Go to Calendar", icon: "calendar", keys: CalendarCommand.calendar.keys) { [model] in
                model.mode = .calendar
            })
            out.append(PaletteItem(id: "day-panel", title: model.showsDayPanel ? "Hide Today Beside Mail" : "Show Today Beside Mail",
                                   icon: "sidebar.right", keys: CalendarCommand.dayPanel.keys) { [model] in model.showsDayPanel.toggle() })
        } else {
            out.append(PaletteItem(id: "mail", title: "Go to Mail", icon: "tray", keys: CalendarCommand.mail.keys) { [model] in model.mode = .mail })
        }
        out.append(PaletteItem(id: "out-of-office", title: "Out of Office…", detail: "Automatic replies and an out-of-office event",
                               icon: "airplane") { [model] in model.openAwaySheet() })
        out.append(PaletteItem(id: "focus-time", title: "Focus Time…", detail: "Block time to work, from the next free 2 hours",
                               icon: "moon") { [model] in model.openFocusSheet() })
        out.append(PaletteItem(id: "add-account", title: "Add Account…", icon: "person.crop.circle.badge.plus", keepsOpen: true) { [model] in
            model.palette = .accounts
        })
        out.append(PaletteItem(id: "settings", title: "Settings…", icon: "gearshape", keys: "⌘,") { [model] in model.openSettings(.general) })
        out.append(PaletteItem(id: "calendar-settings", title: "Calendar Settings…", icon: "gearshape") { [model] in model.openSettings(.calendar) })
        out.append(PaletteItem(id: "sync", title: "Sync Now", icon: "arrow.triangle.2.circlepath") { [model] in model.core.syncNow() })
        out.append(PaletteItem(id: "help", title: Command.help.title, icon: Command.help.icon, keys: Command.help.keys) { [model] in
            model.showsHelp = true
        })
        out.append(PaletteItem(id: "themes", title: "Change Theme…", icon: "paintpalette", keepsOpen: true) { [model] in
            model.palette = .themes
        })
        if let match = matchMacOSItem { out.append(match) }
        out.append(PaletteItem(id: "appearance-settings", title: "Appearance Settings…", icon: "gearshape") { [model] in
            model.openSettings(.appearance)
        })
        return out
    }

    /// While one theme shows always: back to a theme for each macOS appearance.
    private var matchMacOSItem: PaletteItem? {
        guard ThemeStore.shared.choice.fixed != nil else { return nil }
        return PaletteItem(id: "match-macos", title: "Match macOS Appearance", icon: "circle.lefthalf.filled") { [model] in
            let store = ThemeStore.shared
            store.update { $0.matchMacOS() }
            model.notify("Matching macOS again, with \(store.choice.light.name) in light mode and \(store.choice.dark.name) in dark mode.")
        }
    }

    /// Google, Microsoft 365, or iCloud and IMAP: the welcome screen's choices.
    private var accountItems: [PaletteItem] {
        var out: [PaletteItem] = []
        if text.isEmpty {
            out.append(PaletteItem(id: "back", title: "Back to Commands", icon: "chevron.left", keepsOpen: true) { [model] in
                model.palette = .commands
            })
        }
        for kind in NewAccount.allCases {
            out.append(PaletteItem(id: "account-\(kind)", title: "\(kind.name) Account…", icon: kind.icon) { [model] in model.addAccount(kind) })
        }
        return out
    }

    /// Graphite, then Omarchy's themes by name. Choosing one shows it at
    /// once (`ThemeChoice.pick`).
    private var themeItems: [PaletteItem] {
        let store = ThemeStore.shared
        var out: [PaletteItem] = []
        if text.isEmpty {
            out.append(PaletteItem(id: "back", title: "Back to Commands", icon: "chevron.left", keepsOpen: true) { [model] in
                model.palette = .commands
            })
            if let match = matchMacOSItem { out.append(match) }
        }
        for theme in ThemePalette.all {
            out.append(PaletteItem(id: "theme-\(theme.id)", title: theme.name, detail: theme.mode == .dark ? "Dark" : "Light",
                                   icon: "paintpalette", theme: theme) { [model] in
                if store.pick(theme) {
                    model.notify("Always using \(theme.name). Match macOS again in Settings › Appearance.")
                }
            })
        }
        return out
    }

    /// Times for snoozing, sending later or a reminder: what was typed,
    /// read as a time, then the presets.
    private var timeItems: [PaletteItem] {
        var out: [PaletteItem] = []
        let typed = text.trimmingCharacters(in: .whitespaces)
        let (verb, icon) = switch mode {
        case .sendLater: ("Send", "paperplane")
        case .remind: ("Remind me if no reply by", "bell")
        default: ("Snooze until", "clock")
        }
        let choose = { [model, mode] (at: Int64, label: String) in
            switch mode {
            case .sendLater: model.sendCompose(at: at, label: label)
            case .remind: model.composeReminder = AppModel.Reminder(at: at, label: label)
            default: model.snooze(until: at, label: label)
            }
        }
        if !typed.isEmpty, let at = model.core.parseTime(text: typed, utcOffset: model.utcOffset) {
            let label = Self.when(at)
            out.append(PaletteItem(id: "typed", title: "\(verb) \(label)", icon: icon) { choose(at, label) })
        }
        for p in model.core.snoozePresets(utcOffset: model.utcOffset) where typed.isEmpty || p.label.lowercased().contains(typed.lowercased()) {
            let label = Self.when(p.at)
            out.append(PaletteItem(id: "preset-\(p.label)", title: p.label, detail: label, icon: icon) { choose(p.at, label) })
        }
        return out
    }

    private var snippetItems: [PaletteItem] {
        ((try? model.core.snippets()) ?? []).map { s in
            let preview = model.core.bodyPreview(s.body)
            return PaletteItem(id: "snippet-\(s.id)", title: s.name, detail: preview, icon: "text.badge.plus") { [model] in
                model.insertSnippet(s)
            }
        }
    }

    private var moveItems: [PaletteItem] {
        guard let account = model.accounts.first(where: { $0.id == model.cursorRow?.accountId }) else { return [] }
        return account.mailboxes.filter { $0.role != "snoozed" && $0.role != "drafts" }.map { box in
            PaletteItem(id: "move-\(box.id)", title: box.name, detail: box.role ?? "", icon: Sidebar.icon(box.role ?? "")) { [model] in
                model.act(.moveTo(mailboxId: box.id), label: "Moved to \(box.name)")
            }
        }
    }

    /// The account's labels, checked when the conversation has them:
    /// choosing a checked one takes it off, an unchecked one adds it.
    private var labelItems: [PaletteItem] {
        guard let row = model.cursorRow, let labels = try? model.core.threadLabels(threadId: row.threadId) else { return [] }
        return labels.map { label in
            PaletteItem(id: "label-\(label.mailboxId)", title: label.name, icon: "tag", checked: label.applied) { [model] in
                if label.applied {
                    model.act(.removeLabel(mailboxId: label.mailboxId), label: "Removed \(label.name)", threadID: row.threadId)
                } else {
                    model.act(.addLabel(mailboxId: label.mailboxId), label: "Labelled \(label.name)", threadID: row.threadId)
                }
            }
        }
    }

    private static let whenFormat: DateFormatter = {
        let f = DateFormatter()
        f.setLocalizedDateFormatFromTemplate("EEE d MMM HH:mm")
        return f
    }()

    static func when(_ unix: Int64) -> String {
        whenFormat.string(from: Date(timeIntervalSince1970: TimeInterval(unix)))
    }

    /// Subsequence match: higher for a prefix match and for letters close together.
    private func score(_ haystack: String, _ needle: String) -> Int? {
        if haystack.hasPrefix(needle) { return 1000 - haystack.count }
        if haystack.contains(needle) { return 500 - haystack.count }
        var score = 0
        var index = haystack.startIndex
        var last: String.Index?
        for c in needle {
            guard let found = haystack[index...].firstIndex(of: c) else { return nil }
            if let last, haystack.index(after: last) == found { score += 5 }
            last = found
            index = haystack.index(after: found)
        }
        return score - haystack.count
    }

    var body: some View {
        let list = items
        let previewed = list.indices.contains(selected) ? list[selected].theme : nil
        VStack(spacing: 0) {
            HStack(spacing: 12) {
                Image(systemName: icon)
                    .font(.system(size: 15))
                    .foregroundStyle(Color(Theme.text3))
                TextField(placeholder, text: $text)
                    .textFieldStyle(.plain)
                    .font(.system(size: 16))
                    .focused($focused)
                    .onSubmit { choose(list) }
                    .onKeyPress(.downArrow) { selected = min(selected + 1, max(list.count - 1, 0)); return .handled }
                    .onKeyPress(.upArrow) { selected = max(selected - 1, 0); return .handled }
                    // The Mac's ⌫ sends U+007F, which `.delete` (U+0008) doesn't match.
                    .onKeyPress(keys: [.delete, KeyEquivalent("\u{7F}")]) { _ in
                        guard mode == .themes || mode == .accounts, text.isEmpty else { return .ignored }
                        model.palette = .commands
                        return .handled
                    }
                    .onChange(of: text) { selected = 0 }
                if let row = model.cursorRow, [.snooze, .move, .label].contains(mode) {
                    Text(row.subject).lineLimit(1).font(.system(size: Theme.small)).foregroundStyle(Color(Theme.text3)).frame(maxWidth: 180)
                }
            }
            .padding(.horizontal, 18)
            .frame(height: 52)
            .overlay(alignment: .bottom) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }

            ScrollViewReader { proxy in
                ScrollView {
                    VStack(spacing: 0) {
                        ForEach(Array(list.enumerated()), id: \.element.id) { i, item in
                            Button { selected = i; choose(list) } label: { row(item, on: i == selected) }
                                .buttonStyle(.plain)
                                .id(i)
                                .onContinuousHover { phase in
                                    guard case .active = phase, NSEvent.mouseLocation != pointer else { return }
                                    pointer = NSEvent.mouseLocation
                                    guard selected != i else { return }
                                    pointerSelected = true
                                    selected = i
                                }
                        }
                        if list.isEmpty {
                            Text([.snooze, .sendLater, .remind].contains(mode) ? "Try “tomorrow”, “mon 9am” or “in 3 days”"
                                 : mode == .snippets && text.isEmpty ? "No snippets yet. Add them in Settings › Snippets."
                                 : mode == .label && text.isEmpty ? "This account has no labels yet. Make them in Gmail." : "No matches")
                                .foregroundStyle(Color(Theme.text3))
                                .padding(20)
                        }
                    }
                    .padding(.vertical, 8)
                }
                .frame(maxHeight: 380)
                .fixedSize(horizontal: false, vertical: true)
                .onChange(of: selected) { _, i in
                    if pointerSelected { pointerSelected = false } else { proxy.scrollTo(i) }
                }
            }

            HStack(spacing: 16) {
                HStack(spacing: 5) { KeyCap(key: "↑"); KeyCap(key: "↓"); Text("select") }
                HStack(spacing: 5) { KeyCap(key: "↩"); Text("run") }
                HStack(spacing: 5) { KeyCap(key: "esc"); Text("close") }
            }
            .font(.system(size: Theme.small))
            .foregroundStyle(Color(Theme.text3))
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 18)
            .frame(height: 36)
            .overlay(alignment: .top) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
        }
        .frame(width: 560)
        .clipShape(RoundedRectangle(cornerRadius: Theme.radiusPanel))
        .panel()
        // The editor of a message being written takes the keyboard back on
        // the next turn of the run loop; take it again after that.
        .onAppear {
            focused = true
            DispatchQueue.main.async { focused = true }
            pointer = NSEvent.mouseLocation
            if mode == .themes, let i = list.firstIndex(where: { $0.theme == ThemeStore.shared.shown }) { selected = i }
        }
        .onChange(of: previewed) { _, theme in
            // A click chooses and closes in one go; the selection it set arrives here after the save.
            guard model.palette == .themes else { return }
            if let theme { ThemeStore.shared.startPreview(theme) } else { ThemeStore.shared.endPreview() }
        }
        // However the theme list goes away, a theme that wasn't chosen stops showing.
        // Choosing one saves it first, which already ends the preview.
        .onDisappear { if mode == .themes { ThemeStore.shared.endPreview() } }
        .onExitCommand { close() }
    }

    private func row(_ item: PaletteItem, on: Bool) -> some View {
        HStack(spacing: 12) {
            if let theme = item.theme {
                ThemeSwatch(theme: theme).frame(width: 18)
            } else {
                Image(systemName: item.icon).frame(width: 18).foregroundStyle(Color(Theme.text3))
            }
            Text(item.title).foregroundStyle(Color(Theme.text)).lineLimit(1)
            if !item.detail.isEmpty {
                Text(item.detail).font(.system(size: 12)).foregroundStyle(Color(Theme.text3)).lineLimit(1)
            }
            Spacer()
            if let theme = item.theme, let use = ThemeStore.shared.choice.use(of: theme) {
                HStack(spacing: 4) { Image(systemName: "checkmark"); Text(use.label) }
                    .font(.system(size: Theme.small))
                    .foregroundStyle(Color(Theme.text3))
            }
            if item.checked {
                Image(systemName: "checkmark").font(.system(size: 12, weight: .semibold)).foregroundStyle(Color(Theme.text2))
            }
            if let keys = item.keys {
                HStack(spacing: 3) { ForEach(keys.split(separator: " ").map(String.init), id: \.self) { KeyCap(key: KeyCap.shown($0), focused: on) } }
            }
        }
        .font(.system(size: 13))
        .padding(.horizontal, 10)
        .frame(height: 36)
        .selectedRow(on)
        .padding(.horizontal, 8)
        .contentShape(Rectangle())
    }

    private func choose(_ list: [PaletteItem]) {
        guard list.indices.contains(selected) else { return }
        let item = list[selected]
        if !item.keepsOpen { close() }
        item.run()
    }

    private func close() {
        model.palette = nil
        model.returnKeyboard()
    }
}

/// Every shortcut, from the keymap.
struct HelpView: View {
    let model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Keyboard shortcuts").font(.system(size: 15, weight: .semibold))
            LazyVGrid(columns: [GridItem(.flexible(), spacing: 28), GridItem(.flexible())], alignment: .leading, spacing: 8) {
                ForEach(Command.allCases) { c in
                    HStack(spacing: 10) {
                        HStack(spacing: 3) { ForEach(c.keys.split(separator: " ").map(String.init), id: \.self) { KeyCap(key: KeyCap.shown($0)) } }
                            .frame(width: 52, alignment: .leading)
                        Text(c.title).foregroundStyle(Color(Theme.text2))
                    }
                }
                HStack(spacing: 10) {
                    KeyCap(key: "⌘K").frame(width: 52, alignment: .leading)
                    Text("Command Palette").foregroundStyle(Color(Theme.text2))
                }
                HStack(spacing: 10) {
                    KeyCap(key: "⌘N").frame(width: 52, alignment: .leading)
                    Text("New Message").foregroundStyle(Color(Theme.text2))
                }
            }
            .font(.system(size: Theme.body))
            Text("Calendar").font(.system(size: 13, weight: .semibold)).padding(.top, 4)
            LazyVGrid(columns: [GridItem(.flexible(), spacing: 28), GridItem(.flexible())], alignment: .leading, spacing: 8) {
                ForEach(CalendarCommand.allCases) { c in
                    HStack(spacing: 10) {
                        KeyCap(key: KeyCap.shown(c.keys)).frame(width: 52, alignment: .leading)
                        Text(c.title).foregroundStyle(Color(Theme.text2))
                    }
                }
            }
            .font(.system(size: Theme.body))
        }
        .padding(24)
        .frame(width: 600)
        .panel()
        .onExitCommand { model.showsHelp = false }
        .onTapGesture { model.showsHelp = false }
    }
}
