@preconcurrency import KiteFFI
import SwiftUI

struct ContentView: View {
    let model: AppModel

    var body: some View {
        HStack(spacing: 0) {
            Sidebar(model: model)
                .frame(width: Theme.sidebarWidth)
                .background(Color(Theme.side))
                .overlay(alignment: .trailing) { Rectangle().fill(Color(Theme.line2)).frame(width: 1) }
            ListColumn(model: model)
                .frame(minWidth: 420, idealWidth: Theme.listWidth, maxWidth: Theme.listWidth)
                .overlay(alignment: .trailing) { Rectangle().fill(Color(Theme.line2)).frame(width: 1) }
            ReaderColumn(model: model)
                .frame(minWidth: 480, maxWidth: .infinity)
                .background(Color(Theme.reader))
        }
        .background(Color(Theme.bg))
        .background(MainWindowReader())
        .foregroundStyle(Color(Theme.text))
        .font(.system(size: 13))
        .ignoresSafeArea()
        .overlay {
            if model.palette != nil || model.showsHelp {
                Color.black.opacity(0.35)
                    .ignoresSafeArea()
                    .onTapGesture { model.palette = nil; model.showsHelp = false }
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

// MARK: - Sidebar

struct Sidebar: View {
    let model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            ScrollView {
                VStack(alignment: .leading, spacing: 1) {
                    NavRow(icon: "tray", title: "Inbox", count: model.unreadCount("in:inbox"),
                           selected: model.source == .unifiedInbox) { model.source = .unifiedInbox }
                    if !model.roles.isEmpty {
                        GroupLabel("Mailboxes")
                        ForEach(model.roles, id: \.self) { role in
                            NavRow(icon: Self.icon(role), title: role.capitalized,
                                   count: role == "drafts" ? 0 : model.unreadCount("in:\(role)"),
                                   selected: model.source == .role(role)) { model.source = .role(role) }
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
                        GroupLabel("Accounts")
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
                                    if box.role != "inbox" {
                                        Button("Hide Folder") { model.setHidden(box, true) }
                                    }
                                }
                            }
                        }
                    }
                }
                .padding(.horizontal, 10)
                .padding(.top, 52)
                .padding(.bottom, 12)
            }
            .scrollIndicators(.never)
            .overlay(alignment: .top) { TitleBarArea().frame(height: 40).padding(.leading, 80) }
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
    init(_ title: String) { self.title = title }

    var body: some View {
        Text(title)
            .font(.system(size: 11, weight: .semibold))
            .foregroundStyle(Color(Theme.text3))
            .padding(.horizontal, 10)
            .padding(.top, 18)
            .padding(.bottom, 6)
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
        HStack(spacing: 8) {
            Text(title).fontWeight(.medium).lineLimit(1)
            Spacer(minLength: 4)
            if syncing {
                ProgressView().controlSize(.mini)
            }
            Text(provider).font(.system(size: 11)).foregroundStyle(Color(Theme.text3))
        }
        .padding(.horizontal, 10)
        .frame(height: 28)
    }
}

struct SyncFooter: View {
    let model: AppModel

    private var status: String {
        if let error = model.lastError { return error }
        if !model.syncing.isEmpty { return "Syncing…" }
        if let date = model.lastSynced { return "All accounts synced, \(date.formatted(date: .omitted, time: .shortened))" }
        return model.accounts.isEmpty ? "No accounts yet" : "Up to date"
    }

    var body: some View {
        HStack(spacing: 8) {
            Circle().fill(Color(model.lastError == nil ? Theme.text2 : Theme.accent)).frame(width: 6, height: 6)
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

// MARK: - List column

struct ListColumn: View {
    @Bindable var model: AppModel

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 10) {
                Text(model.search.isEmpty ? model.source.title : "Search")
                    .font(.system(size: 15, weight: .semibold))
                    .tracking(-0.15)
                let unread = model.unreadCount(model.query)
                if unread > 0 {
                    Text("\(unread) unread").font(.system(size: 12)).foregroundStyle(Color(Theme.text3))
                }
                Spacer()
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
            ForEach(tabs, id: \.?.id) { c in
                let selected = model.section?.id == c?.id
                Button { model.section = c } label: {
                    HStack(spacing: 5) {
                        Text(c?.name ?? "Everything")
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
            Spacer()
            KeyCap(key: "⇥")
                .padding(.bottom, 8)
                .padding(.trailing, 9)
        }
        .font(.system(size: 12.5))
        .padding(.horizontal, 17)
        .overlay(alignment: .bottom) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
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
                     ? "Kite hasn't downloaded this folder yet. It starts on the next sync."
                     : "Kite is downloading this folder next. Mail shows up here as it arrives.")
                    .font(.system(size: 12))
                    .foregroundStyle(Color(Theme.text3))
                    .multilineTextAlignment(.center)
                    .frame(maxWidth: 260)
            } else {
                Text(model.search.isEmpty ? "No conversations" : "Nothing matches “\(model.search)”")
                    .foregroundStyle(Color(Theme.text3))
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .allowsHitTesting(false)
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
            if !focused { KeyCap(key: "/") }
        }
        .padding(.leading, 10)
        .padding(.trailing, 5)
        .frame(width: 196, height: 28)
        .background(RoundedRectangle(cornerRadius: 8).fill(Color(Theme.fill)))
        .onChange(of: model.focusSearchRequest) { focused = true }
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
    let action: () -> Void
    @State private var hovering = false

    var body: some View {
        Button(action: action) {
            HStack(spacing: 7) {
                Image(systemName: icon).font(.system(size: 12.5))
                Text(title)
                Text(key)
                    .font(.system(size: 10.5, weight: .medium))
                    .foregroundStyle(Color(Theme.text3))
                    .frame(minWidth: 16, minHeight: 16)
                    .overlay(RoundedRectangle(cornerRadius: 4).strokeBorder(Color(Theme.line), lineWidth: 1))
            }
            .font(.system(size: 12.5))
            .foregroundStyle(Color(Theme.text2))
            .padding(.horizontal, 8)
            .frame(height: 28)
            .background(RoundedRectangle(cornerRadius: 7).fill(Color(hovering ? Theme.hover : .clear)))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
    }
}

struct ReaderColumn: View {
    let model: AppModel

    var body: some View {
        VStack(spacing: 0) {
            if model.compose == nil {
            HStack(spacing: 4) {
                if model.openThread != nil {
                    ActionButton(icon: "archivebox", title: "Archive", key: "E") { model.act(.archive, label: "Archived") }
                    ActionButton(icon: "trash", title: "Trash", key: "#") { model.act(.trash, label: "Moved to Trash") }
                    ActionButton(icon: "envelope.badge", title: "Unread", key: "U") { model.act(.markUnread, label: "Marked unread") }
                    ActionButton(icon: "star", title: "Star", key: "S") { model.act(.flag, label: "Starred") }
                }
                Spacer()
                if model.cursor.count > 0 {
                    Text("\(model.cursor.row + 1) of \(model.cursor.count)")
                        .font(.system(size: 12).monospacedDigit())
                        .foregroundStyle(Color(Theme.text3))
                        .padding(.trailing, 6)
                }
                HStack(spacing: 3) { KeyCap(key: "K"); KeyCap(key: "J") }
            }
            .padding(.horizontal, 20)
            .frame(height: Theme.headerHeight)
            .background(TitleBarArea())
            .overlay(alignment: .bottom) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
            }
            if model.compose != nil {
                ComposeView(model: model)
            } else if let thread = model.openThread {
                ReaderView(thread: thread, core: model.core, onAttachment: model.openAttachment)
            } else {
                Text(model.cursor.count == 0 ? "Nothing here" : "")
                    .foregroundStyle(Color(Theme.text3))
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
    }
}
