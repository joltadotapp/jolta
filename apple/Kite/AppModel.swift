@preconcurrency import KiteFFI
import AppKit
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
    var source: ListSource = .unifiedInbox { didSet { section = nil; listChanged(); focusSync() } }
    /// The split-inbox section shown over the inbox; nil shows everything.
    var section: SectionInfo? { didSet { listChanged() } }
    private(set) var sections: [SectionInfo] = []
    private(set) var snoozed: [SnoozeInfo] = []
    /// The thread under the list cursor.
    var cursorRow: ThreadRow?
    /// The ⌘K palette, when open.
    var palette: PaletteMode?
    /// An agent that connected for the first time and waits for approval.
    var pendingAgent: (id: Int64, name: String)?
    /// Bumped when agents act, so the audit view reloads.
    private(set) var agentRevision = 0
    private(set) var mcpStatus = "MCP server not started"
    var showsHelp = false
    var search = "" { didSet { listChanged() } }
    /// Bumped whenever the list's contents may have changed; views reload on it.
    private(set) var listRevision = 0
    private(set) var syncing: Set<Int64> = []
    private(set) var lastSynced: Date?
    private(set) var lastError: String?
    var openThread: ThreadView?
    var selectedThreadID: Int64?
    /// Threads selected in the list, in list order. With more than one,
    /// actions apply to all of them and the reader shows how many.
    private(set) var selectedThreads: [Int64] = []
    var isMultiSelection: Bool { selectedThreads.count > 1 }
    /// Each list's sort, keyed by its source query and kept across launches.
    private var sorts: [String: String] = UserDefaults.standard.dictionary(forKey: "ListSorts") as? [String: String] ?? [:]
    /// The list cursor's row and the list length, for "3 of 120" in the reader.
    var cursor: (row: Int, count: Int) = (0, 0)
    /// Rows in the message list, as the list last counted them.
    var listCount = -1
    /// Asks the message list to take keyboard focus (after search, for example).
    private(set) var focusListRequest = 0
    /// The last action, shown briefly with its undo key.
    private(set) var toast: Toast?
    /// The message being written, shown in place of the reader.
    var compose: ComposeData?
    private var markReadTask: Task<Void, Never>?
    let notifier = Notifier()

    var sort: ListSort {
        get {
            let saved = sorts[source.query] ?? ""
            let key: SortKey = switch saved.trimmingCharacters(in: ["-"]) {
            case "sender": .sender
            case "subject": .subject
            default: .date
            }
            return ListSort(key: key, reversed: saved.hasPrefix("-"))
        }
        set {
            let name = switch newValue.key {
            case .date: "date"
            case .sender: "sender"
            case .subject: "subject"
            }
            sorts[source.query] = (newValue.reversed ? "-" : "") + name
            UserDefaults.standard.set(sorts, forKey: "ListSorts")
            listChanged()
        }
    }

    var showsSections: Bool { source == .unifiedInbox && search.isEmpty && !sections.isEmpty }

    var query: String {
        var parts = [source.query]
        if showsSections, let section { parts.append(section.query) }
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
        startMCP()
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
        // `-KitePalette commands|snooze|move` opens the palette, for screenshots.
        switch defaults.string(forKey: "KitePalette") {
        case "commands": palette = .commands
        case "snooze": palette = .snooze
        case "move": palette = .move
        default: break
        }
        if defaults.bool(forKey: "KiteOpenFirst"), let first = page(offset: 0, limit: 1).first {
            open(threadID: first.threadId)
            if defaults.bool(forKey: "KiteTiming") {
                Task {
                    for _ in 0..<5 {
                        try? await Task.sleep(for: .seconds(1))
                        selectedThreadID = nil
                        open(threadID: first.threadId)
                    }
                }
            }
            // `-KiteAction archive|trash|unread|star` acts on that thread
            // through the same path as the keys.
            let actions: [String: ThreadAction] = ["archive": .archive, "trash": .trash, "unread": .markUnread, "star": .flag]
            if defaults.string(forKey: "KiteAction") == "reply" {
                Task {
                    try? await Task.sleep(for: .seconds(2))
                    reply(.reply, threadID: first.threadId)
                }
            }
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

    /// Reloads the sidebar's data. The counts read the whole store, so they
    /// run off the main thread and only the result lands here.
    func reloadAccounts() {
        let core = core
        Task.detached(priority: .userInitiated) {
            do {
                let accounts = try core.accounts().map { AccountSection(account: $0, mailboxes: try core.mailboxes(accountId: $0.id)) }
                let sections = try core.sections()
                let snoozed = try core.snoozed()
                await MainActor.run { self.apply(accounts: accounts, sections: sections, snoozed: snoozed) }
            } catch {
                await MainActor.run { self.lastError = "\(error)" }
            }
        }
    }

    /// Mailboxes behind the current list that no sync has gone through yet.
    var unsyncedSource: [MailboxInfo] {
        let boxes = accounts.flatMap(\.mailboxes)
        switch source {
        case .unifiedInbox: return boxes.filter { $0.role == "inbox" && !$0.synced }
        case .role(let role): return boxes.filter { $0.role == role && !$0.synced }
        case .mailbox(let id, _): return boxes.filter { $0.id == id && !$0.synced }
        case .allMail: return []
        }
    }

    /// Tells the core which mailbox is on screen, so it downloads that one first.
    private func focusSync() {
        let target: Int64? = switch source {
        case .mailbox(let id, _): id
        default: unsyncedSource.first?.id
        }
        core.focusMailbox(mailboxId: target)
    }

    func setHidden(_ box: MailboxInfo, _ hidden: Bool) {
        do {
            try core.setMailboxHidden(mailboxId: box.id, hidden: hidden)
            if hidden, source == .mailbox(id: box.id, name: box.name) { source = .unifiedInbox }
        } catch {
            show(Toast(text: "\(error)", undo: nil))
        }
    }

    private func apply(accounts: [AccountSection], sections: [SectionInfo], snoozed: [SnoozeInfo]) {
        self.accounts = accounts
        self.sections = sections
        self.snoozed = snoozed
        if let current = section, !sections.contains(where: { $0.id == current.id }) { section = nil }
    }

    static let appGroup = "RWQLHW7V7V.be.example.kite"

    /// The MCP socket lives in the app group container, where the sandboxed
    /// kite-mcp helper can reach it.
    private func startMCP() {
        guard let group = FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: Self.appGroup) else {
            mcpStatus = "MCP is off: no app group container"
            return
        }
        let socket = group.appendingPathComponent("kite.sock").path
        do {
            try core.startMcp(socketPath: socket, utcOffset: utcOffset)
            mcpStatus = "Listening on \(socket)"
        } catch {
            mcpStatus = "MCP is off: \(error)"
        }
    }

    /// Path to the helper agents run, for their MCP configuration.
    var mcpHelperPath: String {
        Bundle.main.bundleURL.appendingPathComponent("Contents/MacOS/kite-mcp").path
    }

    func decideAgent(approve: Bool, grants: [String]) {
        guard let agent = pendingAgent else { return }
        try? core.decideAgent(agentId: agent.id, approve: approve, grants: grants)
        pendingAgent = nil
        agentRevision += 1
        show(Toast(text: approve ? "\(agent.name) can use Kite" : "\(agent.name) was turned away", undo: nil))
    }

    func reviewDraft(_ id: Int64) {
        if let draft = try? core.draft(draftId: id) {
            compose = draft
        }
    }

    func saveSections(_ list: [SectionInfo]) throws {
        try core.setSections(sections: list)
        reloadAccounts()
    }

    /// Cycles the split-inbox tabs: Everything, then each section.
    func nextSection(backwards: Bool = false) {
        guard showsSections else { return }
        let all: [SectionInfo?] = [nil] + sections.map { Optional($0) }
        let i = all.firstIndex { $0?.id == section?.id } ?? 0
        section = all[(i + (backwards ? all.count - 1 : 1)) % all.count]
    }

    func toggleVIP() {
        guard let address = cursorRow?.fromAddr, !address.isEmpty else { return }
        let vip = (try? core.isVip(address: address)) ?? false
        try? core.setVip(address: address, vip: !vip)
        show(Toast(text: vip ? "\(address) is no longer a VIP" : "\(address) is now a VIP", undo: nil))
    }

    func snooze(until: Int64, label: String) {
        act(.snooze(until: until), label: "Snoozed until \(label)")
    }

    var utcOffset: Int32 { Int32(TimeZone.current.secondsFromGMT()) }

    /// Roles that exist in at least one account, in sidebar order.
    var roles: [String] {
        let present = Set(accounts.flatMap { $0.mailboxes.compactMap(\.role) })
        return ["snoozed", "drafts", "sent", "archive", "spam", "trash"].filter(present.contains)
    }

    /// Unread threads for a badge, computed once per list change.
    func unreadCount(_ query: String) -> Int {
        if badgeRevision != listRevision {
            badges.removeAll(keepingCapacity: true)
            badgeRevision = listRevision
        }
        if let n = badges[query] { return n }
        let n = (try? core.queryCount(query: "\(query) is:unread")).map(Int.init) ?? 0
        badges[query] = n
        return n
    }

    @ObservationIgnored private var badges: [String: Int] = [:]
    @ObservationIgnored private var badgeRevision = -1

    func requestListFocus() {
        focusListRequest += 1
    }

    /// Asks the search field to take keyboard focus.
    private(set) var focusSearchRequest = 0

    func requestSearchFocus() {
        focusSearchRequest += 1
    }

    private func listChanged() {
        listRevision += 1
    }

    private func handle(_ event: CoreEvent) {
        switch event {
        case .listsChanged:
            reloadAccounts()
            listChanged()
            refreshOpenThread()
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
        case .agentRequest(let id, let name):
            pendingAgent = (id, name)
            agentRevision += 1
        case .agentDraft(let draftID, let subject):
            show(Toast(text: "An agent wrote a draft: \(subject.isEmpty ? "(no subject)" : subject)", undo: nil, review: draftID))
        case .agentActivity:
            agentRevision += 1
        case .woke(_, _, let subject):
            show(Toast(text: "Back from snooze: \(subject)", undo: nil))
        case .sent(_, let subject):
            show(Toast(text: "Sent: \(subject.isEmpty ? "(no subject)" : subject)", undo: nil))
        case .sendFailed(let message):
            show(Toast(text: "Not sent yet: \(message)", undo: nil))
        }
    }

    func count() -> Int {
        (try? core.listCount(query: query, sort: sort)).map(Int.init) ?? 0
    }

    func page(offset: Int, limit: Int) -> [ThreadRow] {
        (try? core.queryPage(query: query, sort: sort, offset: UInt32(offset), limit: UInt32(limit))) ?? []
    }

    /// Thread ids for these list rows, in list order.
    func threadIDs(rows: IndexSet) -> [Int64] {
        rows.rangeView.flatMap { range in
            (try? core.listThreadIds(query: query, sort: sort, offset: UInt32(range.lowerBound), limit: UInt32(range.count))) ?? []
        }
    }

    /// The rows these threads are on now; threads no longer in the list are left out.
    func rows(of threads: [Int64]) -> IndexSet {
        guard !threads.isEmpty else { return [] }
        let positions = (try? core.listPositions(query: query, sort: sort, threadIds: threads)) ?? []
        return IndexSet(positions.map(Int.init))
    }

    /// Several threads selected: the reader steps aside for a count.
    func select(_ threads: [Int64]) {
        selectedThreads = threads
        guard threads.count > 1 else { return }
        markReadTask?.cancel()
        selectedThreadID = nil
        openThread = nil
    }

    /// Acts on a thread, or on the selection when none is named, and offers undo.
    func act(_ action: ThreadAction, label: String, threadID: Int64? = nil) {
        let threads = threadID.map { [$0] } ?? (isMultiSelection ? selectedThreads : selectedThreadID.map { [$0] } ?? [])
        guard !threads.isEmpty else { return }
        do {
            let tokens = try core.perform(action: action, threadIds: threads, context: source.actionContext)
            let text = threads.count > 1 ? "\(label): \(threads.count) conversations" : label
            if !tokens.isEmpty { show(Toast(text: text, undo: .action(tokens))) }
        } catch {
            show(Toast(text: "\(error)", undo: nil))
        }
    }

    func undo() {
        switch toast?.undo {
        case .action(let tokens):
            try? core.undo(tokens: tokens)
            show(Toast(text: "Undone", undo: nil))
        case .send(let token):
            if let back = try? core.cancelSend(token: token) {
                compose = back
                show(Toast(text: "Sending cancelled", undo: nil))
            } else {
                show(Toast(text: "Already sent", undo: nil))
            }
        case nil:
            break
        }
    }

    // MARK: Compose

    func startCompose() {
        compose = (try? core.newCompose()) ?? .blank
    }

    func reply(_ mode: ReplyMode, threadID: Int64? = nil) {
        guard let thread = threadID ?? selectedThreadID else { return }
        let offset = Int32(TimeZone.current.secondsFromGMT())
        Task {
            do {
                compose = try await core.replyCompose(threadId: thread, mode: mode, utcOffset: offset)
            } catch {
                show(Toast(text: "\(error)", undo: nil))
            }
        }
    }

    func sendCompose() {
        guard let draft = compose else { return }
        do {
            let token = try core.send(compose: draft)
            compose = nil
            requestListFocus()
            show(Toast(text: "Sending in 10 seconds", undo: .send(token)))
        } catch {
            show(Toast(text: "\(error)", undo: nil))
        }
    }

    func saveDraft() {
        guard let draft = compose, !draft.isEmpty else { return }
        if let id = try? core.saveDraft(compose: draft), compose?.draftId == nil {
            compose?.draftId = id
        }
    }

    /// Closes the editor and keeps what was written as a draft.
    func closeCompose() {
        saveDraft()
        if compose?.isEmpty == false { show(Toast(text: "Saved to Drafts", undo: nil)) }
        compose = nil
        requestListFocus()
    }

    func discardCompose() {
        if let id = compose?.draftId { try? core.discardDraft(draftId: id) }
        compose = nil
        requestListFocus()
    }

    /// Opens an attachment with its default app, from a temporary copy.
    func openAttachment(message: Int64, index: UInt32) {
        Task {
            do {
                let file = try await core.attachment(messageId: message, index: index)
                let dir = FileManager.default.temporaryDirectory.appendingPathComponent("attachments/\(message)-\(index)")
                try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
                let url = dir.appendingPathComponent(file.name.replacingOccurrences(of: "/", with: "-"))
                try Data(file.bytes).write(to: url)
                NSWorkspace.shared.open(url)
            } catch {
                show(Toast(text: "\(error)", undo: nil))
            }
        }
    }

    private func show(_ toast: Toast) {
        self.toast = toast
        let id = toast.id
        Task {
            try? await Task.sleep(for: .seconds(toast.review != nil ? 12 : toast.undo == nil ? 3 : 6))
            if self.toast?.id == id { self.toast = nil }
        }
    }

    /// `-KiteTiming YES` appends launch and open timings to timing.txt in the data folder.
    static func timing(_ line: String) {
        guard UserDefaults.standard.bool(forKey: "KiteTiming") else { return }
        let url = URL(fileURLWithPath: dataDirectory + "/timing.txt")
        let text = line + "\n"
        if let handle = try? FileHandle(forWritingTo: url) {
            handle.seekToEndOfFile()
            handle.write(Data(text.utf8))
            try? handle.close()
        } else {
            try? text.write(to: url, atomically: true, encoding: .utf8)
        }
    }

    /// Milliseconds since the process started.
    static var sinceLaunch: Double {
        var info = kinfo_proc()
        var size = MemoryLayout<kinfo_proc>.stride
        var mib: [Int32] = [CTL_KERN, KERN_PROC, KERN_PROC_PID, getpid()]
        sysctl(&mib, 4, &info, &size, nil, 0)
        let start = info.kp_proc.p_un.__p_starttime
        let started = Double(start.tv_sec) + Double(start.tv_usec) / 1e6
        return (Date().timeIntervalSince1970 - started) * 1000
    }

    /// Re-renders the open conversation after mail changed (a reply came
    /// in, a message moved). Leaves read state alone: a refresh is not a read.
    private func refreshOpenThread() {
        guard let threadID = selectedThreadID, openThread?.threadId == threadID, compose == nil else { return }
        Task {
            guard let view = try? await core.openThread(threadId: threadID, appCss: ReaderStyle.css, utcOffset: utcOffset),
                  selectedThreadID == threadID, view.html != openThread?.html else { return }
            openThread = view
        }
    }

    /// An empty list has nothing under the cursor, so the reader goes blank.
    func closeThread() {
        markReadTask?.cancel()
        selectedThreadID = nil
        selectedThreads = []
        openThread = nil
        cursorRow = nil
        cursor = (0, 0)
    }

    func open(threadID: Int64) {
        selectedThreadID = threadID
        selectedThreads = [threadID]
        let opening = ContinuousClock.now
        markReadTask?.cancel()
        markReadTask = Task {
            try? await Task.sleep(for: .seconds(1))
            guard !Task.isCancelled, selectedThreadID == threadID else { return }
            _ = try? core.perform(action: .markRead, threadIds: [threadID], context: source.actionContext)
        }
        let offset = Int32(TimeZone.current.secondsFromGMT())
        Task {
            do {
                let view = try await core.openThread(threadId: threadID, appCss: ReaderStyle.css, utcOffset: offset)
                if selectedThreadID == threadID {
                    openThread = view
                    let ms = (ContinuousClock.now - opening).components
                    Self.timing(String(format: "open thread %lld: %.1f ms (%d messages)", threadID,
                                       Double(ms.seconds) * 1000 + Double(ms.attoseconds) / 1e15, view.messageCount))
                }
            } catch {
                lastError = "\(error)"
            }
        }
    }
}

struct Toast: Equatable {
    enum Undo: Equatable {
        case action([Int64])
        case send(Int64)
    }

    let id = UUID()
    let text: String
    let undo: Undo?
    /// A draft to open for review (an agent wrote it).
    var review: Int64?
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

/// Errors from the core read as their message, wherever the app shows one.
extension CoreError: @retroactive CustomStringConvertible {
    public var description: String {
        switch self {
        case .Query(let message), .NotFound(let message), .SignIn(let message), .Failed(let message): message
        }
    }
}
