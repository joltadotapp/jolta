@preconcurrency import KiteFFI
import Foundation
import Observation

/// What the message list shows. Every list is a query in the core's search language.
enum ListSource: Hashable {
    case unifiedInbox
    /// One role across all accounts: sent, drafts, archive, trash, spam.
    case role(String)
    case mailbox(id: Int64, name: String)
    case allMail

    var query: String {
        switch self {
        case .unifiedInbox: "in:inbox"
        case .role(let role): "in:\(role)"
        case .mailbox(let id, _): "mailbox:\(id)"
        case .allMail: ""
        }
    }

    /// Where an action taken from this list moves messages out of.
    var actionContext: ListContext {
        switch self {
        case .unifiedInbox, .allMail: .inbox
        case .role(let role): .role(role: role)
        case .mailbox(let id, _): .mailbox(mailboxId: id)
        }
    }

    var title: String {
        switch self {
        case .unifiedInbox: "Inbox"
        case .role(let role): role.capitalized
        case .mailbox(_, let name): name
        case .allMail: "All mail"
        }
    }
}

/// The light category bar above the inbox. Becomes the user's own split-inbox
/// sections in M7; until then, three fixed ones.
enum Category: String, CaseIterable, Identifiable {
    case all = "Everything"
    case people = "People"
    case newsletters = "Newsletters"

    var id: String { rawValue }

    var query: String {
        switch self {
        case .all: ""
        case .people: "-has:list"
        case .newsletters: "has:list"
        }
    }
}

struct AccountSection: Identifiable {
    let account: AccountInfo
    let mailboxes: [MailboxInfo]
    var id: Int64 { account.id }
    var title: String { account.displayName.isEmpty ? account.address : account.displayName }
}

@MainActor
@Observable
final class AppModel {
    let core: Core
    private(set) var accounts: [AccountSection] = []
    var source: ListSource = .unifiedInbox { didSet { category = .all; listChanged() } }
    var category: Category = .all { didSet { listChanged() } }
    var search = "" { didSet { listChanged() } }
    /// Bumped whenever the list's contents may have changed; views reload on it.
    private(set) var listRevision = 0
    private(set) var syncing: Set<Int64> = []
    private(set) var lastSynced: Date?
    private(set) var lastError: String?
    var openThread: ThreadView?
    var selectedThreadID: Int64?
    /// The list cursor's row and the list length, for "3 of 120" in the reader.
    var cursor: (row: Int, count: Int) = (0, 0)
    /// Asks the message list to take keyboard focus (after search, for example).
    private(set) var focusListRequest = 0
    /// The last action, shown briefly with its undo key.
    private(set) var toast: Toast?
    private var markReadTask: Task<Void, Never>?
    let notifier = Notifier()

    var showsCategories: Bool { source == .unifiedInbox && search.isEmpty }

    var query: String {
        var parts = [source.query]
        if showsCategories { parts.append(category.query) }
        parts.append(search.trimmingCharacters(in: .whitespaces))
        return parts.filter { !$0.isEmpty }.joined(separator: " ")
    }

    init() throws {
        let bridge = EventBridge()
        core = try Core.open(dataDir: Self.dataDirectory, listener: bridge)
        Task { [weak self] in
            for await event in bridge.events {
                self?.handle(event)
            }
        }
        reloadAccounts()
        try core.start()
        notifier.onOpen = { [weak self] threadID in
            self?.source = .unifiedInbox
            self?.open(threadID: threadID)
        }
        if accounts.contains(where: { $0.account.provider != "Synthetic" }) {
            notifier.requestPermission()
        }
        // Launch arguments for checking the app from scripts:
        // `-KiteSearch "subject:x" -KiteOpenFirst YES`.
        let defaults = UserDefaults.standard
        if let query = defaults.string(forKey: "KiteSearch") {
            source = .allMail
            search = query
        }
        if defaults.bool(forKey: "KiteOpenFirst"), let first = page(offset: 0, limit: 1).first {
            open(threadID: first.threadId)
            // `-KiteAction archive|trash|unread|star` acts on that thread
            // through the same path as the keys.
            let actions: [String: ThreadAction] = ["archive": .archive, "trash": .trash, "unread": .markUnread, "star": .flag]
            if let name = defaults.string(forKey: "KiteAction"), let action = actions[name] {
                Task {
                    try? await Task.sleep(for: .seconds(2))
                    act(action, label: name, threadID: first.threadId)
                }
            }
        }
    }

    /// `~/Library/Application Support/Kite` in the real home folder. Inside the
    /// sandbox `NSHomeDirectory()` points at the container, so ask the user database.
    static var dataDirectory: String {
        let home = getpwuid(getuid()).flatMap { String(cString: $0.pointee.pw_dir) } ?? NSHomeDirectory()
        return home + "/Library/Application Support/Kite"
    }

    func reloadAccounts() {
        do {
            accounts = try core.accounts().map { AccountSection(account: $0, mailboxes: try core.mailboxes(accountId: $0.id)) }
        } catch {
            lastError = "\(error)"
        }
    }

    /// Roles that exist in at least one account, in sidebar order.
    var roles: [String] {
        let present = Set(accounts.flatMap { $0.mailboxes.compactMap(\.role) })
        return ["drafts", "sent", "archive", "spam", "trash"].filter(present.contains)
    }

    func unreadCount(_ query: String) -> Int {
        (try? core.queryCount(query: "\(query) is:unread")).map(Int.init) ?? 0
    }

    func requestListFocus() {
        focusListRequest += 1
    }

    private func listChanged() {
        listRevision += 1
    }

    private func handle(_ event: CoreEvent) {
        switch event {
        case .listsChanged:
            reloadAccounts()
            listChanged()
        case .syncStarted(let id):
            syncing.insert(id)
        case .syncFinished(let id):
            syncing.remove(id)
            lastSynced = .now
            lastError = nil
        case .syncFailed(let id, let message):
            syncing.remove(id)
            lastError = message
        case .newMail(_, let threadID, let from, let subject):
            notifier.post(threadID: threadID, from: from, subject: subject)
        case .actionFailed(let message):
            show(Toast(text: "Couldn't sync that change: \(message)", undo: nil))
        }
    }

    func count() -> Int {
        (try? core.queryCount(query: query)).map(Int.init) ?? 0
    }

    func page(offset: Int, limit: Int) -> [ThreadRow] {
        (try? core.queryPage(query: query, offset: UInt32(offset), limit: UInt32(limit))) ?? []
    }

    /// Acts on a thread (the selected one by default) and offers undo.
    func act(_ action: ThreadAction, label: String, threadID: Int64? = nil) {
        guard let thread = threadID ?? selectedThreadID else { return }
        do {
            let token = try core.perform(action: action, threadId: thread, context: source.actionContext)
            if token != nil { show(Toast(text: label, undo: token)) }
        } catch {
            show(Toast(text: "\(error)", undo: nil))
        }
    }

    func undo() {
        guard let token = toast?.undo else { return }
        try? core.undo(token: token)
        show(Toast(text: "Undone", undo: nil))
    }

    private func show(_ toast: Toast) {
        self.toast = toast
        let id = toast.id
        Task {
            try? await Task.sleep(for: .seconds(toast.undo == nil ? 3 : 6))
            if self.toast?.id == id { self.toast = nil }
        }
    }

    func open(threadID: Int64) {
        selectedThreadID = threadID
        markReadTask?.cancel()
        markReadTask = Task {
            try? await Task.sleep(for: .seconds(1))
            guard !Task.isCancelled, selectedThreadID == threadID else { return }
            _ = try? core.perform(action: .markRead, threadId: threadID, context: source.actionContext)
        }
        let offset = Int32(TimeZone.current.secondsFromGMT())
        Task {
            do {
                let view = try await core.openThread(threadId: threadID, appCss: ReaderStyle.css, utcOffset: offset)
                if selectedThreadID == threadID {
                    openThread = view
                }
            } catch {
                lastError = "\(error)"
            }
        }
    }
}

struct Toast: Equatable {
    let id = UUID()
    let text: String
    let undo: Int64?
}

/// Receives core events on the core's threads and hands them to the main actor.
final class EventBridge: CoreListener, @unchecked Sendable {
    let events: AsyncStream<CoreEvent>
    private let continuation: AsyncStream<CoreEvent>.Continuation

    init() {
        (events, continuation) = AsyncStream.makeStream(of: CoreEvent.self, bufferingPolicy: .bufferingNewest(64))
    }

    func onEvent(event: CoreEvent) {
        continuation.yield(event)
    }
}
