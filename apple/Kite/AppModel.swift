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
        core.syncAll()
        // Launch arguments for checking the app from scripts:
        // `-KiteSearch "subject:x" -KiteOpenFirst YES`.
        let defaults = UserDefaults.standard
        if let query = defaults.string(forKey: "KiteSearch") {
            source = .allMail
            search = query
        }
        if defaults.bool(forKey: "KiteOpenFirst"), let first = page(offset: 0, limit: 1).first {
            open(threadID: first.threadId)
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
        case .syncFinished(let id, _):
            syncing.remove(id)
            lastSynced = .now
        case .syncFailed(let id, let message):
            syncing.remove(id)
            lastError = message
        }
    }

    func count() -> Int {
        (try? core.queryCount(query: query)).map(Int.init) ?? 0
    }

    func page(offset: Int, limit: Int) -> [ThreadRow] {
        (try? core.queryPage(query: query, offset: UInt32(offset), limit: UInt32(limit))) ?? []
    }

    func open(threadID: Int64) {
        selectedThreadID = threadID
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
