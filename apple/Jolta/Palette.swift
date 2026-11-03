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
    /// The theme for the Mac's dark or light appearance, previewed as the selection moves.
    case themes
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
    let run: () -> Void
}

/// ⌘K: every command, mailbox and section by name. The same panel picks a
/// snooze time (H), a destination folder (V, L) and a theme.
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
        case .themes: "Choose a theme"
        }
    }

    private var icon: String {
        switch mode {
        case .snooze: "clock"
        case .sendLater: "paperplane"
        case .remind: "bell"
        case .snippets: "text.badge.plus"
        case .themes: "paintpalette"
        case .commands, .move: "magnifyingglass"
        }
    }

    private var items: [PaletteItem] {
        let all: [PaletteItem]
        switch mode {
        case .commands: all = commandItems
        case .snooze, .sendLater, .remind: return timeItems
        case .move: all = moveItems
        case .snippets: all = snippetItems
        case .themes: all = themeItems
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
        out.append(PaletteItem(id: "sync", title: "Sync now", icon: "arrow.triangle.2.circlepath") { [model] in model.core.syncNow() })
        out.append(PaletteItem(id: "help", title: Command.help.title, icon: Command.help.icon, keys: Command.help.keys) { [model] in
            model.showsHelp = true
        })
        out.append(PaletteItem(id: "themes", title: "Change Theme…", icon: "paintpalette", keepsOpen: true) { [model] in
            model.palette = .themes
        })
        return out
    }

    /// Graphite, then Omarchy's themes by name. Choosing one makes it the
    /// theme for its own mode, whichever mode the Mac is in.
    private var themeItems: [PaletteItem] {
        let store = ThemeStore.shared
        var out: [PaletteItem] = []
        if text.isEmpty {
            out.append(PaletteItem(id: "back", title: "Back to Commands", icon: "chevron.left", keepsOpen: true) { [model] in
                model.palette = .commands
            })
        }
        for theme in ThemePalette.all {
            out.append(PaletteItem(id: "theme-\(theme.id)", title: theme.name, detail: theme.mode == .dark ? "Dark" : "Light",
                                   icon: "paintpalette", theme: theme) { [model] in
                let system = store.systemMode
                store.save(theme)
                if theme.mode != system {
                    let shown = system == .dark ? store.dark : store.light
                    model.notify("\(theme.name) is your \(theme.mode.rawValue) theme. Your Mac is in \(system.rawValue) mode, so Jolta shows \(shown.name).")
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
            let preview = model.core.bodySpans(body: s.body).map(\.text).joined().split(separator: "\n").first.map(String.init) ?? ""
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
                        guard mode == .themes, text.isEmpty else { return .ignored }
                        model.palette = .commands
                        return .handled
                    }
                    .onChange(of: text) { selected = 0 }
                if let row = model.cursorRow, mode == .snooze || mode == .move {
                    Text(row.subject).lineLimit(1).font(.system(size: 11.5)).foregroundStyle(Color(Theme.text3)).frame(maxWidth: 180)
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
                                 : mode == .snippets && text.isEmpty ? "No snippets yet. Add them in Settings › Snippets." : "Nothing matches")
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
            .font(.system(size: 11.5))
            .foregroundStyle(Color(Theme.text3))
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 18)
            .frame(height: 36)
            .overlay(alignment: .top) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
        }
        .frame(width: 560)
        .background(RoundedRectangle(cornerRadius: 14).fill(Color(Theme.raise)))
        .clipShape(RoundedRectangle(cornerRadius: 14))
        .shadow(color: .black.opacity(0.45), radius: 32, y: 16)
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
                HStack(spacing: -3) {
                    swatch(theme.tokens.bg)
                    swatch(theme.tokens.text)
                    swatch(theme.tokens.accent)
                }
                .frame(width: 18)
            } else {
                Image(systemName: item.icon).frame(width: 18).foregroundStyle(Color(Theme.text3))
            }
            Text(item.title).foregroundStyle(Color(Theme.text)).lineLimit(1)
            if !item.detail.isEmpty {
                Text(item.detail).font(.system(size: 12)).foregroundStyle(Color(Theme.text3)).lineLimit(1)
            }
            Spacer()
            if let theme = item.theme, ThemeStore.shared.isSaved(theme) {
                HStack(spacing: 4) { Image(systemName: "checkmark"); Text("In use") }
                    .font(.system(size: 11.5))
                    .foregroundStyle(Color(Theme.text3))
            }
            if let keys = item.keys {
                HStack(spacing: 3) { ForEach(keys.split(separator: " ").map(String.init), id: \.self) { KeyCap(key: $0.uppercased(), focused: on) } }
            }
        }
        .font(.system(size: 13))
        .padding(.horizontal, 10)
        .frame(height: 36)
        .background(RoundedRectangle(cornerRadius: 8).fill(on ? Color(Theme.fill) : .clear))
        .overlay(alignment: .leading) {
            if on { Image(systemName: "arrowtriangle.right.fill").font(.system(size: 8)).foregroundStyle(Color(Theme.accent)).offset(x: -9) }
        }
        .padding(.horizontal, 8)
        .contentShape(Rectangle())
    }

    private func swatch(_ color: UInt32) -> some View {
        Circle()
            .fill(Color(nsColor: NSColor(hex: color)))
            .overlay(Circle().strokeBorder(Color(Theme.line), lineWidth: 1))
            .frame(width: 8, height: 8)
    }

    private func choose(_ list: [PaletteItem]) {
        guard list.indices.contains(selected) else { return }
        let item = list[selected]
        if !item.keepsOpen { close() }
        item.run()
    }

    private func close() {
        model.palette = nil
        model.requestListFocus()
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
                        HStack(spacing: 3) { ForEach(c.keys.split(separator: " ").map(String.init), id: \.self) { KeyCap(key: $0.uppercased()) } }
                            .frame(width: 52, alignment: .leading)
                        Text(c.title).foregroundStyle(Color(Theme.text2))
                    }
                }
                HStack(spacing: 10) {
                    KeyCap(key: "⌘K").frame(width: 52, alignment: .leading)
                    Text("Command palette").foregroundStyle(Color(Theme.text2))
                }
                HStack(spacing: 10) {
                    KeyCap(key: "⌘N").frame(width: 52, alignment: .leading)
                    Text("New message").foregroundStyle(Color(Theme.text2))
                }
            }
            .font(.system(size: 12.5))
        }
        .padding(24)
        .frame(width: 600)
        .background(RoundedRectangle(cornerRadius: 14).fill(Color(Theme.raise)))
        .shadow(color: .black.opacity(0.45), radius: 32, y: 16)
        .onExitCommand { model.showsHelp = false }
        .onTapGesture { model.showsHelp = false }
    }
}
