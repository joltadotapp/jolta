@preconcurrency import JoltaFFI
import SwiftUI

struct ContentView: View {
    let model: AppModel
    @AppStorage("ListWidth") private var listWidth = Double(Theme.listWidth)
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Group {
            if model.mode == .calendar {
                CalendarModeView(model: model)
            } else {
                mail
            }
        }
        .sheet(isPresented: Binding(get: { model.showsCalendarIntro }, set: { if !$0 { model.finishCalendarIntro() } })) {
            CalendarIntroSheet(model: model)
        }
        .overlay(alignment: .trailing) {
            // Over the list and reader; the list stays underneath for the keys.
            if model.isInboxZero, model.mode == .mail {
                GeometryReader { geo in
                    let panel: CGFloat = model.showsDayPanel ? 300 : 0
                    InboxZero(compose: model.startCompose)
                        .frame(width: geo.size.width - Theme.sidebarWidth - panel)
                        .frame(maxWidth: .infinity, alignment: .trailing)
                        .padding(.trailing, panel)
                }
                .transition(.opacity)
            }
        }
        .animation(.easeInOut(duration: 0.4), value: model.isInboxZero)
        .onChange(of: model.settingsRequest, initial: true) { _, page in
            // The settings window takes the request and switches to the page.
            if page != nil { openWindow(id: "settings") }
        }
        .background(Color(Theme.bg))
        .background(MainWindowReader())
        .confirmation(Binding(get: { model.confirmEmpty.map(emptyTrash) }, set: { if $0 == nil { model.confirmEmpty = nil } }))
        .foregroundStyle(Color(Theme.text))
        .font(.system(size: 13))
        .ignoresSafeArea()
        .overlay {
            if model.palette != nil || model.showsHelp || model.findTime != nil {
                // Light while picking a theme, so the preview behind the palette shows.
                Color.black.opacity(model.palette == .themes ? 0.08 : 0.35)
                    .ignoresSafeArea()
                    .onTapGesture { model.palette = nil; model.showsHelp = false; model.findTime = nil }
            }
        }
        .overlay {
            if let request = model.findTime {
                FindTimePanel(model: model, request: request).id(request)
            }
        }
        .overlay {
            if let agent = model.pendingAgent {
                ZStack {
                    Color.black.opacity(0.35).ignoresSafeArea()
                    AgentApproval(name: agent.name) { approve, grants in model.decideAgent(approve: approve, grants: grants) }
                }
            }
        }
        .overlay(alignment: .top) {
            if let mode = model.palette {
                PaletteView(model: model, mode: mode)
                    .id(mode)
                    .padding(.top, 110)
            } else if model.showsHelp {
                HelpView(model: model).padding(.top, 110)
            }
        }
    }
}

/// The line between list and reader. Dragging it sets the list's width;
/// a double click goes back to the default. An AppKit view, so it gets the
/// clicks ahead of the message list's table underneath.
struct ColumnResizer: NSViewRepresentable {
    @Binding var width: Double

    func makeNSView(context: Context) -> Handle { Handle() }

    func updateNSView(_ handle: Handle, context: Context) {
        handle.width = width
        handle.onChange = { width = $0 }
    }

    final class Handle: NSView {
        var width = 0.0
        var onChange: (Double) -> Void = { _ in }
        private var start: (x: CGFloat, width: Double)?

        override var mouseDownCanMoveWindow: Bool { false }

        override func resetCursorRects() {
            addCursorRect(bounds, cursor: .columnResize)
        }

        override func draw(_ dirtyRect: NSRect) {
            Theme.line2.setFill()
            NSRect(x: bounds.midX - 0.5, y: 0, width: 1, height: bounds.height).fill()
        }

        override func mouseDown(with event: NSEvent) {
            if event.clickCount == 2 {
                onChange(Double(Theme.listWidth))
            } else {
                start = (event.locationInWindow.x, width)
            }
        }

        override func mouseDragged(with event: NSEvent) {
            guard let start else { return }
            onChange(min(max(start.width + event.locationInWindow.x - start.x, 380), 900))
        }

        override func mouseUp(with event: NSEvent) {
            start = nil
        }
    }
}

extension ContentView {
    private var mail: some View {
        HStack(spacing: 0) {
            Sidebar(model: model)
                .frame(width: Theme.sidebarWidth)
                .background(Color(Theme.side))
                .overlay(alignment: .trailing) { Rectangle().fill(Color(Theme.line2)).frame(width: 1) }
            // The chosen width, sized before the reader (which keeps its 480 pt
            // minimum): a narrow window takes width from the list first.
            ListColumn(model: model)
                .frame(minWidth: 380, idealWidth: listWidth, maxWidth: listWidth)
                .overlay(alignment: .trailing) { ColumnResizer(width: $listWidth).frame(width: 9).offset(x: 4) }
                .layoutPriority(1)
                .zIndex(1)
            ReaderColumn(model: model)
                .frame(minWidth: model.showsDayPanel ? 360 : 480, maxWidth: .infinity)
                .background(Color(Theme.reader))
            if model.showsDayPanel {
                DayPanel(model: model)
                    .frame(width: 300)
                    .transition(.move(edge: .trailing))
            }
        }
        .animation(.snappy(duration: 0.2), value: model.showsDayPanel)
    }

    private func emptyTrash(_ list: ListSource) -> Confirmation {
        let accounts = model.trashFolders(list).compactMap { box in model.accounts.first { $0.id == box.accountId }?.title }
        return Confirmation(
            title: "Empty Trash?",
            message: "Everything in Trash in \(ListFormatter.localizedString(byJoining: accounts)) is deleted for good, on this Mac and on the server. You can't undo this.",
            action: "Empty Trash") { [model] in model.emptyTrash(list) }
    }
}

// MARK: - Sidebar

struct Sidebar: View {
    let model: AppModel
    @AppStorage("AccountsCollapsed") private var accountsCollapsed = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            // The window buttons sit here; the list scrolls below them, not under them.
            TitleBarArea().frame(height: 44)
                .overlay(alignment: .topLeading) { ModeSwitcher(model: model).padding(.top, 5).padding(.leading, ModeSwitcher.leading) }
            ScrollView {
                VStack(alignment: .leading, spacing: 1) {
                    NavRow(icon: "tray", title: "Inbox", count: model.unreadCount("in:inbox"),
                           selected: model.source == .unifiedInbox) { model.source = .unifiedInbox }
                        .contextMenu { ListActions(model: model, list: .unifiedInbox) }
                    NavRow(icon: "star", title: "Starred", count: 0,
                           selected: model.source == .starred) { model.source = .starred }
                    if !model.roles.isEmpty {
                        GroupLabel("Mailboxes")
                        ForEach(model.roles, id: \.self) { role in
                            NavRow(icon: Self.icon(role), title: role.capitalized,
                                   count: role == "drafts" ? 0 : model.unreadCount("in:\(role)"),
                                   selected: model.source == .role(role)) { model.source = .role(role) }
                                .contextMenu { ListActions(model: model, list: .role(role)) }
                        }
                    }
                    if !model.scheduled.isEmpty {
                        GroupLabel("Scheduled")
                        ForEach(model.scheduled, id: \.token) { s in
                            Button { model.editScheduled(s) } label: {
                                HStack(spacing: 10) {
                                    Text(Self.wake(s.sendAt))
                                        .font(.system(size: 11, design: .monospaced))
                                        .foregroundStyle(Color(Theme.text3))
                                        .frame(width: 74, alignment: .leading)
                                    Text(s.subject.isEmpty ? "(no subject)" : s.subject).lineLimit(1).foregroundStyle(Color(Theme.text2))
                                }
                                .font(.system(size: 12))
                                .padding(.horizontal, 10)
                                .frame(height: 24)
                                .contentShape(Rectangle())
                            }
                            .buttonStyle(.plain)
                            .help("Jolta sends it then if it's running, otherwise as soon as it opens. Click to edit it.")
                        }
                    }
                    if !model.snoozed.isEmpty {
                        GroupLabel("Next to wake")
                        ForEach(model.snoozed.prefix(5), id: \.threadId) { s in
                            Button { model.source = .role("snoozed"); model.open(threadID: s.threadId) } label: {
                                HStack(spacing: 10) {
                                    Text(Self.wake(s.wakeAt))
                                        .font(.system(size: 11, design: .monospaced))
                                        .foregroundStyle(Color(Theme.text3))
                                        .frame(width: 74, alignment: .leading)
                                    Text(s.subject).lineLimit(1).foregroundStyle(Color(Theme.text2))
                                }
                                .font(.system(size: 12))
                                .padding(.horizontal, 10)
                                .frame(height: 24)
                                .contentShape(Rectangle())
                            }
                            .buttonStyle(.plain)
                        }
                    }
                    if !model.accounts.isEmpty {
                        GroupLabel("Accounts", collapsed: $accountsCollapsed,
                                   count: model.accounts.flatMap(\.mailboxes).filter { !$0.hidden && ($0.role == nil || $0.role == "inbox") }
                                       .reduce(0) { $0 + Int($1.unread) })
                    }
                    if !model.accounts.isEmpty, !accountsCollapsed {
                        ForEach(model.accounts) { section in
                            let folders = section.mailboxes.filter { $0.role == nil || $0.role == "inbox" }
                            let hidden = folders.filter(\.hidden)
                            AccountHeader(title: section.title, provider: section.account.provider,
                                          syncing: model.syncing.contains(section.id))
                                .contextMenu {
                                    if hidden.isEmpty {
                                        Text("No hidden folders")
                                    } else {
                                        Menu("Show Hidden Folder") {
                                            ForEach(hidden, id: \.id) { box in
                                                Button(box.name) { model.setHidden(box, false) }
                                            }
                                        }
                                    }
                                }
                            ForEach(folders.filter { !$0.hidden }, id: \.id) { box in
                                NavRow(icon: nil, title: box.name, count: Int(box.unread),
                                       selected: model.source == .mailbox(id: box.id, name: box.name), indent: 26) {
                                    model.source = .mailbox(id: box.id, name: box.name)
                                }
                                .contextMenu {
                                    ListActions(model: model, list: .mailbox(id: box.id, name: box.name))
                                    if box.role != "inbox" {
                                        Divider()
                                        Button("Hide Folder") { model.setHidden(box, true) }
                                    }
                                }
                            }
                        }
                    }
                }
                .padding(.horizontal, 10)
                .padding(.top, 8)
                .padding(.bottom, 12)
            }
            .scrollIndicators(.never)
            SyncFooter(model: model)
        }
    }

    private static let wakeTime: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "HH:mm"
        return f
    }()

    private static let wakeDay: DateFormatter = {
        let f = DateFormatter()
        f.setLocalizedDateFormatFromTemplate("EEE HH:mm")
        return f
    }()

    /// `14:00` today, `Fri 08` this week, `3 Nov` later.
    static func wake(_ unix: Int64) -> String {
        let date = Date(timeIntervalSince1970: TimeInterval(unix))
        if Calendar.current.isDateInToday(date) { return wakeTime.string(from: date) }
        if date.timeIntervalSinceNow < 6 * 86_400 { return wakeDay.string(from: date) }
        return Theme.shortDate(unix)
    }

    static func icon(_ role: String) -> String {
        switch role {
        case "snoozed": "clock"
        case "inbox": "tray"
        case "drafts": "pencil"
        case "sent": "paperplane"
        case "archive": "archivebox"
        case "spam": "xmark.bin"
        case "trash": "trash"
        default: "folder"
        }
    }
}

struct GroupLabel: View {
    let title: String
    /// A group that folds away by clicking its label.
    var collapsed: Binding<Bool>?
    /// Unread mail inside the group, shown while it is folded away.
    var count = 0
    @State private var hovering = false

    init(_ title: String, collapsed: Binding<Bool>? = nil, count: Int = 0) {
        self.title = title
        self.collapsed = collapsed
        self.count = count
    }

    var body: some View {
        if let collapsed {
            Button { withAnimation(.easeOut(duration: 0.15)) { collapsed.wrappedValue.toggle() } } label: {
                HStack(spacing: 5) {
                    label
                    // Shown on hover, as in Finder, and always while folded so the hidden rows are findable.
                    Image(systemName: "chevron.right")
                        .font(.system(size: 8.5, weight: .bold))
                        .rotationEffect(.degrees(collapsed.wrappedValue ? 0 : 90))
                        .opacity(hovering || collapsed.wrappedValue ? 1 : 0)
                    Spacer()
                    if collapsed.wrappedValue, count > 0 {
                        Text("\(count)").font(.system(size: 11).monospacedDigit())
                    }
                }
                .foregroundStyle(Color(Theme.text3))
                .padding(.horizontal, 10)
                .padding(.top, 18)
                .padding(.bottom, 6)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .onHover { hovering = $0 }
            .help(collapsed.wrappedValue ? "Show \(title.lowercased())" : "Hide \(title.lowercased())")
        } else {
            label
                .foregroundStyle(Color(Theme.text3))
                .padding(.horizontal, 10)
                .padding(.top, 18)
                .padding(.bottom, 6)
        }
    }

    private var label: some View {
        Text(title).font(.system(size: 11, weight: .semibold))
    }
}

struct NavRow: View {
    let icon: String?
    let title: String
    let count: Int
    let selected: Bool
    var indent: CGFloat = 0
    let action: () -> Void
    @State private var hovering = false

    var body: some View {
        Button(action: action) {
            HStack(spacing: 10) {
                if let icon {
                    Image(systemName: icon)
                        .font(.system(size: 13))
                        .frame(width: 16)
                        .foregroundStyle(Color(selected ? Theme.accent : Theme.text3))
                }
                Text(title)
                    .fontWeight(selected ? .medium : .regular)
                    .foregroundStyle(Color(selected ? Theme.text : Theme.text2))
                    .lineLimit(1)
                Spacer(minLength: 4)
                if count > 0 {
                    Text("\(count)")
                        .font(.system(size: 12).monospacedDigit())
                        .foregroundStyle(Color(Theme.text3))
                }
            }
            .padding(.leading, 10 + indent)
            .padding(.trailing, 10)
            .frame(height: 28)
            .contentShape(Rectangle())
            .background(RoundedRectangle(cornerRadius: 7).fill(Color(selected ? Theme.fill : hovering ? Theme.hover : .clear)))
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
    }
}

struct AccountHeader: View {
    let title: String
    let provider: String
    let syncing: Bool

    var body: some View {
        // The address names the account; a long one keeps its domain, which
        // tells one person's accounts apart, and the provider steps aside.
        ViewThatFits(in: .horizontal) {
            row(showsProvider: true)
            row(showsProvider: false)
        }
        .padding(.horizontal, 10)
        .frame(height: 28)
        .help("\(title), \(provider)")
    }

    private func row(showsProvider: Bool) -> some View {
        HStack(spacing: 8) {
            Text(title).fontWeight(.medium).lineLimit(1).truncationMode(.middle)
                .fixedSize(horizontal: showsProvider, vertical: false)
            Spacer(minLength: 4)
            if syncing {
                ProgressView().controlSize(.mini)
            }
            if showsProvider {
                Text(provider).font(.system(size: 11)).foregroundStyle(Color(Theme.text3)).fixedSize()
            }
        }
    }
}

struct SyncFooter: View {
    let model: AppModel

    private var status: String {
        if let (id, error) = model.syncErrors.min(by: { $0.key < $1.key }) {
            let name = model.accounts.first { $0.account.id == id }?.account.address ?? "An account"
            let more = model.syncErrors.count > 1 ? " (and \(model.syncErrors.count - 1) more)" : ""
            return "\(name): \(error)\(more)"
        }
        if let error = model.lastError { return error }
        if !model.syncing.isEmpty { return "Syncing…" }
        if let date = model.lastSynced { return "All accounts synced, \(date.formatted(date: .omitted, time: .shortened))" }
        return model.accounts.isEmpty ? "No accounts yet" : "Up to date"
    }

    var body: some View {
        HStack(spacing: 8) {
            Circle().fill(Color(model.lastError == nil && model.syncErrors.isEmpty ? Theme.text2 : Theme.accent)).frame(width: 6, height: 6)
            Text(status).lineLimit(2)
        }
        .font(.system(size: 11.5))
        .foregroundStyle(Color(Theme.text3))
        .padding(.horizontal, 20)
        .padding(.vertical, 12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .overlay(alignment: .top) { Rectangle().fill(Color(Theme.line2)).frame(height: 1).padding(.horizontal, 10) }
    }
}

/// What a folder's context menu offers for the whole list.
struct ListActions: View {
    let model: AppModel
    let list: ListSource

    var body: some View {
        Button("Mark All as Read") { model.markAllRead(list) }
        if !model.trashFolders(list).isEmpty {
            Button("Empty Trash…") { model.confirmEmpty = list }
        }
    }
}

// MARK: - List column

struct ListColumn: View {
    @Bindable var model: AppModel

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 10) {
                Text(model.search.isEmpty ? model.source.title : "Search")
                    .font(.system(size: 15, weight: .semibold))
                    .tracking(-0.15)
                    .lineLimit(1)
                    .layoutPriority(1)
                let unread = model.unreadCount(model.query)
                // A narrow list drops the unread count rather than wrap it.
                ViewThatFits(in: .horizontal) {
                    HStack(spacing: 0) {
                        if unread > 0 {
                            Text("\(unread) unread").font(.system(size: 12)).foregroundStyle(Color(Theme.text3)).fixedSize()
                        }
                        Spacer(minLength: 0)
                    }
                    Spacer(minLength: 0)
                }
                SortMenu(model: model)
                SearchField(model: model)
            }
            .padding(.leading, 22)
            .padding(.trailing, 20)
            .frame(height: Theme.headerHeight)
            .background(TitleBarArea())
            if model.showsSections {
                SectionBar(model: model)
            }
            MessageList(model: model)
                .overlay {
                    if model.listCount == 0 { EmptyList(model: model) }
                }
                .overlay(alignment: .bottom) {
                    if let toast = model.toast {
                        ToastView(toast: toast, undo: { model.undo() }, review: { model.reviewDraft($0) })
                            .padding(.bottom, 14)
                            .transition(.move(edge: .bottom).combined(with: .opacity))
                    }
                }
                .animation(.snappy(duration: 0.22), value: model.toast)
            KeyFooter()
        }
    }
}

struct SectionBar: View {
    @Bindable var model: AppModel

    var body: some View {
        let tabs: [SectionInfo?] = [nil] + model.sections.map { Optional($0) }
        HStack(spacing: 0) {
            // Tabs that outgrow a narrow list scroll sideways instead of wrapping.
            ScrollView(.horizontal) {
                HStack(spacing: 0) { self.tabs(tabs) }
            }
            .scrollIndicators(.never)
            .scrollBounceBehavior(.basedOnSize)
            ViewThatFits(in: .horizontal) {
                HStack(spacing: 5) {
                    KeyCap(key: "⇥")
                    Text("next").font(.system(size: 11.5)).foregroundStyle(Color(Theme.text3))
                }
                .fixedSize()
                .padding(.leading, 8)
                Color.clear.frame(width: 0)
            }
            .padding(.bottom, 8)
            .padding(.trailing, 9)
        }
        .font(.system(size: 12.5))
        .padding(.horizontal, 17)
        .overlay(alignment: .bottom) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
    }

    private func tabs(_ tabs: [SectionInfo?]) -> some View {
            ForEach(tabs, id: \.?.id) { c in
                let selected = model.section?.id == c?.id
                Button { model.section = c } label: {
                    HStack(spacing: 5) {
                        Text(c?.name ?? "Everything")
                            .lineLimit(1)
                            .fontWeight(selected ? .medium : .regular)
                            .foregroundStyle(Color(selected ? Theme.text : Theme.text3))
                        let n = model.unreadCount(["in:inbox", c?.query ?? ""].joined(separator: " "))
                        if n > 0 {
                            Text("\(n)").font(.system(size: 11.5).monospacedDigit()).foregroundStyle(Color(Theme.text3))
                        }
                    }
                    .padding(.top, 6)
                    .padding(.bottom, 8)
                    .overlay(alignment: .bottom) {
                        Rectangle().fill(Color(selected ? Theme.text2 : .clear)).frame(height: 1.5)
                    }
                    // The whole tab is the target, not just the letters.
                    .padding(.horizontal, 9)
                    .contentShape(Rectangle())
                }
                .buttonStyle(.plain)
            }
    }
}

/// What an empty list means: nothing there, nothing found, or not
/// downloaded yet (a first sync works through folders one at a time).
struct EmptyList: View {
    let model: AppModel

    var body: some View {
        let pending = model.unsyncedSource
        VStack(spacing: 8) {
            if !pending.isEmpty {
                ProgressView().controlSize(.small)
                Text("Downloading \(model.source.title)…").foregroundStyle(Color(Theme.text2))
                Text(model.syncing.isEmpty
                     ? "Jolta hasn't downloaded this folder yet. It starts on the next sync."
                     : "Jolta is downloading this folder next. Mail shows up here as it arrives.")
                    .font(.system(size: 12))
                    .foregroundStyle(Color(Theme.text3))
                    .multilineTextAlignment(.center)
                    .frame(maxWidth: 260)
            } else {
                Text(model.search.isEmpty ? "No conversations" : "Nothing matches “\(model.search)”")
                    .foregroundStyle(Color(Theme.text3))
                if model.searchLooksLikeRequest {
                    Text("Press ⌘J in the search field to search with AI.")
                        .font(.system(size: 12))
                        .foregroundStyle(Color(Theme.text3))
                }
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .allowsHitTesting(false)
    }
}

/// Date, sender or subject; choosing the current key again flips its direction.
struct SortMenu: View {
    @Bindable var model: AppModel

    var body: some View {
        let sort = model.sort
        Menu {
            ForEach([SortKey.date, .sender, .subject], id: \.self) { key in
                Toggle(Self.name(key), isOn: Binding(
                    get: { sort.key == key },
                    set: { _ in model.sort = ListSort(key: key, reversed: sort.key == key && !sort.reversed) }))
            }
            Divider()
            Picker("Direction", selection: Binding(get: { sort.reversed }, set: { model.sort = ListSort(key: sort.key, reversed: $0) })) {
                Text(sort.key == .date ? "Newest First" : "A to Z").tag(false)
                Text(sort.key == .date ? "Oldest First" : "Z to A").tag(true)
            }
            .pickerStyle(.inline)
            .labelsHidden()
        } label: {
            HStack(spacing: 4) {
                Text(Self.name(sort.key))
                Image(systemName: sort.reversed ? "arrow.up" : "arrow.down").font(.system(size: 9.5, weight: .semibold))
            }
            .font(.system(size: 12))
            .foregroundStyle(Color(Theme.text3))
        }
        .menuStyle(.button)
        .buttonStyle(.plain)
        .menuIndicator(.hidden)
        .fixedSize()
        .help("Sort conversations")
    }

    static func name(_ key: SortKey) -> String {
        switch key {
        case .date: "Date"
        case .sender: "Sender"
        case .subject: "Subject"
        }
    }
}

struct SearchField: View {
    @Bindable var model: AppModel
    @FocusState private var focused: Bool

    var body: some View {
        HStack(spacing: 7) {
            Image(systemName: "magnifyingglass").font(.system(size: 11.5)).foregroundStyle(Color(Theme.text3))
            TextField("Search", text: $model.search)
                .textFieldStyle(.plain)
                .font(.system(size: 12))
                .focused($focused)
                .onSubmit { model.requestListFocus() }
                .onExitCommand {
                    model.search = ""
                    model.requestListFocus()
                }
            if model.aiSearching {
                ProgressView().controlSize(.mini).help("Searching with AI…")
            } else if focused && model.searchLooksLikeRequest {
                KeyCap(key: "⌘J").help("Search with AI")
            } else if !focused {
                KeyCap(key: "/")
            }
        }
        .padding(.leading, 10)
        .padding(.trailing, 5)
        .frame(minWidth: 130, idealWidth: 196, maxWidth: 196)
        .frame(height: 28)
        .background(RoundedRectangle(cornerRadius: 8).fill(Color(Theme.fill)))
        .onChange(of: model.focusSearchRequest) { focused = true }
        .onChange(of: focused) { _, now in model.searchFocused = now }
    }
}

struct ToastView: View {
    let toast: Toast
    let undo: () -> Void
    var review: (Int64) -> Void = { _ in }

    var body: some View {
        HStack(spacing: 12) {
            Text(toast.text).lineLimit(1)
            if toast.undo != nil {
                Button(action: undo) {
                    HStack(spacing: 6) { Text("Undo"); KeyCap(key: "Z", focused: true) }
                }
                .buttonStyle(.plain)
            }
            if let draft = toast.review {
                Button("Review") { review(draft) }.buttonStyle(.plain).fontWeight(.medium)
            }
        }
        .font(.system(size: 12.5))
        .foregroundStyle(Color(Theme.text))
        .fixedSize()
        .padding(.horizontal, 14)
        .frame(height: 36)
        .background(RoundedRectangle(cornerRadius: 10).fill(Color(Theme.raise)))
        .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Color(Theme.line), lineWidth: 1))
        .shadow(color: .black.opacity(0.25), radius: 16, y: 6)
    }
}

struct KeyFooter: View {
    var body: some View {
        HStack(spacing: 14) {
            hint(["J", "K"], "move")
            hint(["E"], "archive")
            hint(["R"], "reply")
            hint(["Z"], "undo")
            Spacer()
            HStack(spacing: 5) { KeyCap(key: "⌘K", focused: true); Text("commands") }.fixedSize()
        }
        .font(.system(size: 11.5))
        .foregroundStyle(Color(Theme.text3))
        .padding(.horizontal, 20)
        .frame(height: 34)
        .overlay(alignment: .top) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
    }

    private func hint(_ keys: [String], _ label: String) -> some View {
        HStack(spacing: 5) {
            ForEach(keys, id: \.self) { KeyCap(key: $0) }
            Text(label)
        }
        .fixedSize()
    }
}

// MARK: - Reader column

struct ActionButton: View {
    let icon: String
    let title: String
    let key: String
    var showsTitle = true
    var iconTint: Color?
    let action: () -> Void
    @State private var hovering = false

    var body: some View {
        Button(action: action) {
            HStack(spacing: 7) {
                Image(systemName: icon).font(.system(size: 12.5)).foregroundStyle(iconTint ?? Color(Theme.text2))
                if showsTitle { Text(title) }
                Text(key)
                    .font(.system(size: 10.5, weight: .medium))
                    .foregroundStyle(Color(Theme.text3))
                    .frame(minWidth: 16, minHeight: 16)
                    .overlay(RoundedRectangle(cornerRadius: 4).strokeBorder(Color(Theme.line), lineWidth: 1))
            }
            .font(.system(size: 12.5))
            .foregroundStyle(Color(Theme.text2))
            .padding(.horizontal, 7)
            .frame(height: 28)
            .background(RoundedRectangle(cornerRadius: 7).fill(Color(hovering ? Theme.hover : .clear)))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
        .help(title)
    }
}

struct ReaderColumn: View {
    let model: AppModel

    /// `titles` names the triage buttons, `replyTitles` the reply ones, which
    /// lose their words first when the reader narrows.
    private func actions(titles: Bool, replyTitles: Bool) -> some View {
        HStack(spacing: 4) {
            ActionButton(icon: "archivebox", title: "Archive", key: "E", showsTitle: titles) { model.act(.archive, label: "Archived") }
            ActionButton(icon: "trash", title: "Trash", key: "#", showsTitle: titles) { model.act(.trash, label: "Moved to Trash") }
            ActionButton(icon: "envelope.badge", title: "Unread", key: "U", showsTitle: titles) { model.act(.markUnread, label: "Marked unread") }
            let starred = model.cursorRow?.flagged == true
            ActionButton(icon: starred ? "star.fill" : "star", title: starred ? "Unstar" : "Star", key: "S", showsTitle: titles,
                         iconTint: starred ? Color(Theme.star) : nil) { model.run(.star) }
            if !model.isMultiSelection {
                Rectangle().fill(Color(Theme.line)).frame(width: 1, height: 16).padding(.horizontal, 2)
                ActionButton(icon: "arrowshape.turn.up.left", title: "Reply", key: "R", showsTitle: replyTitles) { model.reply(.reply) }
                ActionButton(icon: "arrowshape.turn.up.right", title: "Forward", key: "F", showsTitle: replyTitles) { model.reply(.forward) }
            }
        }
        .fixedSize()
    }

    private func bar(titles: Bool, replyTitles: Bool, position: Bool, jk: Bool) -> some View {
        HStack(spacing: 4) {
            if model.openThread != nil || model.isMultiSelection {
                actions(titles: titles, replyTitles: replyTitles)
            }
            Spacer(minLength: 12)
            if position, model.cursor.count > 0, !model.isMultiSelection {
                Text("\(model.cursor.row + 1) of \(model.cursor.count)")
                    .font(.system(size: 12).monospacedDigit())
                    .foregroundStyle(Color(Theme.text3))
                    .fixedSize()
                    .padding(.trailing, 6)
            }
            if jk { HStack(spacing: 3) { KeyCap(key: "K"); KeyCap(key: "J") } }
        }
    }

    var body: some View {
        VStack(spacing: 0) {
            if model.compose == nil {
            // A narrowing reader sheds, in turn, the reply buttons' words, the
            // position, the J K hint and the triage buttons' words.
            ViewThatFits(in: .horizontal) {
                bar(titles: true, replyTitles: true, position: true, jk: true)
                bar(titles: true, replyTitles: false, position: true, jk: true)
                bar(titles: true, replyTitles: false, position: false, jk: true)
                bar(titles: true, replyTitles: false, position: false, jk: false)
                bar(titles: false, replyTitles: false, position: true, jk: true)
                bar(titles: false, replyTitles: false, position: false, jk: false)
            }
            .padding(.horizontal, 20)
            .frame(height: Theme.headerHeight)
            .background(TitleBarArea())
            .overlay(alignment: .bottom) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
            }
            if model.compose != nil {
                ComposeView(model: model)
            } else if model.isMultiSelection {
                VStack(spacing: 6) {
                    Text("\(model.selectedThreads.count) conversations selected")
                        .font(.system(size: 15, weight: .medium))
                        .foregroundStyle(Color(Theme.text2))
                    Text("Archive, Trash, Unread, Star and VIP (!) act on all of them.")
                        .font(.system(size: 12))
                        .foregroundStyle(Color(Theme.text3))
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else if let thread = model.openThread {
                if let summary = model.summary, summary.threadID == thread.threadId {
                    SummaryCard(summary: summary, label: model.ai.label) { model.openSettings(.ai) }
                        .animation(.easeInOut(duration: 0.2), value: summary.state)
                }
                ReaderView(thread: thread, model: model, onAttachment: model.openAttachment,
                           onSaveAll: { model.saveAttachments(message: $0) }, onUnsubscribe: model.unsubscribe)
            } else {
                Text(model.cursor.count == 0 ? "Nothing here" : "")
                    .foregroundStyle(Color(Theme.text3))
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
    }
}
