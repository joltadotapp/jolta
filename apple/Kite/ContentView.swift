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
        .foregroundStyle(Color(Theme.text))
        .font(.system(size: 13))
        .ignoresSafeArea()
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
                    if !model.accounts.isEmpty {
                        GroupLabel("Accounts")
                        ForEach(model.accounts) { section in
                            AccountHeader(title: section.title, provider: section.account.provider,
                                          syncing: model.syncing.contains(section.id))
                            ForEach(section.mailboxes.filter { $0.role == nil || $0.role == "inbox" }, id: \.id) { box in
                                NavRow(icon: nil, title: box.name, count: Int(box.unread),
                                       selected: model.source == .mailbox(id: box.id, name: box.name), indent: 26) {
                                    model.source = .mailbox(id: box.id, name: box.name)
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
            SyncFooter(model: model)
        }
    }

    static func icon(_ role: String) -> String {
        switch role {
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
            if model.showsCategories {
                CategoryBar(model: model)
            }
            MessageList(model: model)
                .overlay(alignment: .bottom) {
                    if let toast = model.toast {
                        ToastView(toast: toast) { model.undo() }
                            .padding(.bottom, 14)
                            .transition(.move(edge: .bottom).combined(with: .opacity))
                    }
                }
                .animation(.snappy(duration: 0.22), value: model.toast)
            KeyFooter()
        }
    }
}

struct CategoryBar: View {
    @Bindable var model: AppModel

    var body: some View {
        HStack(spacing: 18) {
            ForEach(Category.allCases) { c in
                let selected = model.category == c
                Button { model.category = c } label: {
                    HStack(spacing: 5) {
                        Text(c.rawValue)
                            .fontWeight(selected ? .medium : .regular)
                            .foregroundStyle(Color(selected ? Theme.text : Theme.text3))
                        let n = model.unreadCount(["in:inbox", c.query].joined(separator: " "))
                        if n > 0 {
                            Text("\(n)").font(.system(size: 11.5).monospacedDigit()).foregroundStyle(Color(Theme.text3))
                        }
                    }
                    .padding(.bottom, 8)
                    .overlay(alignment: .bottom) {
                        Rectangle().fill(Color(selected ? Theme.text2 : .clear)).frame(height: 1.5)
                    }
                }
                .buttonStyle(.plain)
            }
            Spacer()
            KeyCap(key: "⇥")
                .padding(.bottom, 8)
        }
        .font(.system(size: 12.5))
        .padding(.horizontal, 26)
        .overlay(alignment: .bottom) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
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
        .background {
            Button("") { focused = true }.keyboardShortcut("/", modifiers: []).hidden()
        }
    }
}

struct ToastView: View {
    let toast: Toast
    let undo: () -> Void

    var body: some View {
        HStack(spacing: 12) {
            Text(toast.text).lineLimit(1)
            if toast.undo != nil {
                Button(action: undo) {
                    HStack(spacing: 6) { Text("Undo"); KeyCap(key: "Z", focused: true) }
                }
                .buttonStyle(.plain)
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
            .overlay(alignment: .bottom) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
            }
            if model.compose != nil {
                ComposeView(model: model)
            } else if let thread = model.openThread {
                ReaderView(thread: thread, onAttachment: model.openAttachment)
            } else {
                Text(model.cursor.count == 0 ? "Nothing here" : "")
                    .foregroundStyle(Color(Theme.text3))
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
    }
}
