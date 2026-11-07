@preconcurrency import JoltaFFI
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
    /// Starred (flagged) conversations from every folder except Trash and Spam.
    case starred

    var query: String {
        switch self {
        case .unifiedInbox: "in:inbox"
        case .starred: "is:starred -in:trash -in:spam"
        case .role(let role): "in:\(role)"
        case .mailbox(let id, _): "mailbox:\(id)"
        case .allMail: ""
        }
    }

    /// Where an action taken from this list moves messages out of.
    var actionContext: ListContext {
        switch self {
        case .unifiedInbox, .allMail, .starred: .inbox
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
        case .starred: "Starred"
        }
    }
}

struct AccountSection: Identifiable {
    let account: AccountInfo
    let mailboxes: [MailboxInfo]
    var id: Int64 { account.id }
    /// The address: the display name is the sender name on mail, the same
    /// for most of one person's accounts.
    var title: String { account.address }
}

@MainActor
@Observable
final class AppModel {
    let core: Core
    let calendar: CalendarModel
    /// Mail or the full calendar (⌘1, ⌘2).
    var mode: AppMode = .mail { didSet { if mode != oldValue { modeChanged() } } }
    /// Today's calendar beside the mail, toggled with ⇧C.
    var showsDayPanel = UserDefaults.standard.bool(forKey: AppModel.key("DayPanel")) {
        didSet { UserDefaults.standard.set(showsDayPanel, forKey: Self.key("DayPanel")) }
    }
    /// The first open of calendar mode with no calendar on asks which to show.
    var showsCalendarIntro = false
    @ObservationIgnored var reminderTask: Task<Void, Never>?
    private(set) var accounts: [AccountSection] = []
    var source: ListSource = .unifiedInbox { didSet { section = nil; listChanged(); focusSync() } }
    /// The split-inbox section shown over the inbox; nil shows everything.
    var section: SectionInfo? { didSet { listChanged() } }
    private(set) var sections: [SectionInfo] = []
    private(set) var snoozed: [SnoozeInfo] = []
    /// Messages set to go out later, soonest first.
    private(set) var scheduled: [ScheduledInfo] = []
    /// A reminder chosen for the message being written.
    var composeReminder: Reminder?
    /// When the message being written was scheduled to go, while it is open
    /// again from the sidebar. Closing it keeps it scheduled.
    var composeSchedule: Reminder?

    struct Reminder: Equatable {
        let at: Int64
        let label: String
    }
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
    /// Each account's last sync failure, until that account syncs again.
    private(set) var syncErrors: [Int64: String] = [:]
    var openThread: ThreadView?
    /// How the open card's next answer goes out, and the answer going out now.
    var answerChoices = AnswerChoices()
    var pendingAnswer: InvitationAnswer?
    /// The card whose calendar copy Remove from calendar, Add to calendar
    /// or Update in your calendar is changing now.
    var pendingRemoval: Int64?
    /// The Add to calendar menu's calendars and the one highlighted.
    var addChoices: [AddChoice] = []
    var addHighlight: Int64?
    /// The proposal whose Decline proposal is going out now.
    var pendingDecline: Int64?
    /// Set by P: the card whose Propose new time panel the reader opens.
    var proposeFor: Int64?
    /// The time Find a time picked for the Propose new time panel.
    var proposedSlot: TimeSlot?
    /// Find a time, while its panel is up.
    var findTime: FindTimeRequest?
    /// The note field, the scope menu, the calendar menu or the propose
    /// panel under the open card, when one is up.
    var rsvpPanel: RsvpPanel?
    /// The card the choices, the panel and the pending answer belong to.
    var rsvpMessage: Int64?
    /// Messages of the open conversation the user expanded (true) or
    /// collapsed, as the reader reports them.
    var expandedMessages: [Int64: Bool] = [:]
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
    /// Compose lives in mail mode; starting one from calendar mode goes there.
    var compose: ComposeData? { didSet { if compose != nil, mode == .calendar { mode = .mail } } }
    /// The AI settings, and the models that can run on this Mac.
    private(set) var ai: AiSettingsInfo
    private(set) var localModels: [LocalModelInfo] = []
    /// The open conversation's summary.
    private(set) var summary: ConversationSummary?
    private var summaryTask: Task<Void, Never>?
    /// A settings page something asked to show; the window opens on it.
    var settingsRequest: SettingsPage?
    /// The search field has the keyboard, so ⌘J searches with AI.
    var searchFocused = false
    private(set) var aiSearching = false
    private var markReadTask: Task<Void, Never>?
    let notifier = Notifier()
    private let power = PowerWatch()

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

    /// An inbox, fully downloaded, with nothing in it: the calm screen
    /// takes the list and the reader. Not for searches or sections.
    var isInboxZero: Bool {
        guard listCount == 0, search.isEmpty, section == nil, compose == nil, unsyncedSource.isEmpty else { return false }
        switch source {
        case .unifiedInbox: return !accounts.isEmpty
        case .mailbox(let id, _): return accounts.flatMap(\.mailboxes).first { $0.id == id }?.role == "inbox"
        default: return false
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
        calendar = CalendarModel(core: core)
        ai = try core.aiSettings()
        localModels = core.localModels()
        Task { [weak self] in
            for await event in bridge.events {
                self?.handle(event)
            }
        }
        reloadAccounts()
        try core.start()
        if !Self.isTestData { startMCP() }
        power.start(core: core)
        AddressBook.load(into: core)
        notifier.onOpen = { [weak self] threadID in
            self?.mode = .mail
            self?.source = .unifiedInbox
            self?.open(threadID: threadID)
        }
        if accounts.contains(where: { $0.account.provider != "Synthetic" }) {
            notifier.requestPermission()
        }
        startCalendar()
        // Launch arguments for checking the app from scripts:
        // `-JoltaSearch "subject:x" -JoltaOpenFirst YES`.
        let defaults = UserDefaults.standard
        if let query = defaults.string(forKey: "JoltaSearch") {
            source = .allMail
            search = query
        }
        // `-JoltaEmpty <mailbox id>` asks to empty that one Trash folder.
        if let id = Int64(defaults.string(forKey: "JoltaEmpty") ?? "") {
            Task {
                try? await Task.sleep(for: .seconds(2))
                if let box = accounts.flatMap(\.mailboxes).first(where: { $0.id == id }) {
                    source = .mailbox(id: box.id, name: box.name)
                    confirmEmpty = source
                }
            }
        }
        // `-JoltaSettings ai|calendar|appearance` opens the settings window on that page, for screenshots.
        switch defaults.string(forKey: "JoltaSettings") {
        case "ai": settingsRequest = .ai
        case "appearance": settingsRequest = .appearance
        case "calendar": settingsRequest = .calendar
        default: break
        }
        // `-JoltaTheme tokyo-night` previews that theme, for screenshots.
        if let id = defaults.string(forKey: "JoltaTheme"), let theme = ThemePalette.named(id) { ThemeStore.shared.startPreview(theme) }
        // `-JoltaPalette commands|snooze|move|label|themes` opens the palette, for screenshots.
        switch defaults.string(forKey: "JoltaPalette") {
        case "commands": palette = .commands
        case "snooze": palette = .snooze
        case "move": palette = .move
        case "label": palette = .label
        case "themes": palette = .themes
        default: break
        }
        if defaults.bool(forKey: "JoltaOpenFirst"), let first = page(offset: 0, limit: 1).first {
            open(threadID: first.threadId)
            if defaults.bool(forKey: "JoltaTiming") {
                Task {
                    for _ in 0..<5 {
                        try? await Task.sleep(for: .seconds(1))
                        selectedThreadID = nil
                        open(threadID: first.threadId)
                    }
                }
            }
            // `-JoltaAction archive|trash|unread|star` acts on that thread
            // through the same path as the keys.
            let actions: [String: ThreadAction] = ["archive": .archive, "trash": .trash, "unread": .markUnread, "star": .flag]
            if defaults.string(forKey: "JoltaAction") == "reply" {
                Task {
                    try? await Task.sleep(for: .seconds(2))
                    reply(.reply, threadID: first.threadId)
                }
            }
            if let name = defaults.string(forKey: "JoltaAction"), let action = actions[name] {
                Task {
                    try? await Task.sleep(for: .seconds(2))
                    act(action, label: name, threadID: first.threadId)
                }
            }
        }
    }

    /// `~/Library/Application Support/Jolta` in the real home folder. Inside the
    /// sandbox `NSHomeDirectory()` points at the container, so ask the user database.
    static var dataDirectory: String {
        let home = getpwuid(getuid()).flatMap { String(cString: $0.pointee.pw_dir) } ?? NSHomeDirectory()
        // `-JoltaTestData YES` runs on a separate store of test mail, so checks never open real mail.
        return home + "/Library/Application Support/Jolta" + (testFolder.map { "/" + $0 } ?? "")
    }

    static var isTestData: Bool { testFolder != nil }

    /// A preference key of its own per test folder, so checks never change
    /// the preferences of Jolta as Koen uses it.
    static func key(_ name: String) -> String {
        testFolder.map { "\($0).\(name)" } ?? name
    }

    /// The test store's folder: `Test` for `-JoltaTestData YES`, or the name
    /// given, so each UI test can start from its own fresh folder.
    private static var testFolder: String? {
        guard let value = UserDefaults.standard.string(forKey: "JoltaTestData"), !value.isEmpty else { return nil }
        if ["YES", "1", "true"].contains(value) { return "Test" }
        if ["NO", "0", "false"].contains(value) { return nil }
        let allowed = CharacterSet.alphanumerics.union(CharacterSet(charactersIn: "-"))
        return value.unicodeScalars.allSatisfy(allowed.contains) ? value : "Test"
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
                let scheduled = try core.scheduled()
                await MainActor.run {
                    self.apply(accounts: accounts, sections: sections, snoozed: snoozed)
                    self.scheduled = scheduled
                }
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
        case .allMail, .starred: return []
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

    static let appGroup = "RWQLHW7V7V.app.jolta"

    /// The MCP socket lives in the app group container, where the sandboxed
    /// jolta-mcp helper can reach it.
    private func startMCP() {
        guard let group = FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: Self.appGroup) else {
            mcpStatus = "MCP is off: no app group container"
            return
        }
        let socket = group.appendingPathComponent("jolta.sock").path
        do {
            try core.startMcp(socketPath: socket, utcOffset: utcOffset)
            mcpStatus = "Listening on \(socket)"
        } catch {
            mcpStatus = "MCP is off: \(error)"
        }
    }

    /// Path to the helper agents run, for their MCP configuration.
    var mcpHelperPath: String {
        Bundle.main.bundleURL.appendingPathComponent("Contents/MacOS/jolta-mcp").path
    }

    func decideAgent(approve: Bool, grants: [String]) {
        guard let agent = pendingAgent else { return }
        try? core.decideAgent(agentId: agent.id, approve: approve, grants: grants)
        pendingAgent = nil
        agentRevision += 1
        show(Toast(text: approve ? "\(agent.name) can use Jolta" : "\(agent.name) was turned away", undo: nil))
    }

    func reviewDraft(_ id: Int64) {
        if let draft = try? core.draft(draftId: id) {
            compose = draft
        }
    }

    /// Saves the sections and returns them as stored, read right away:
    /// `reloadAccounts` lands later, and a page that read `sections` before
    /// it did dropped the row being edited.
    @discardableResult
    func saveSections(_ list: [SectionInfo]) throws -> [SectionInfo] {
        try core.setSections(sections: list)
        let stored = try core.sections()
        sections = stored
        reloadAccounts()
        return stored
    }

    /// Cycles the split-inbox tabs: Everything, then each section.
    func nextSection(backwards: Bool = false) {
        guard showsSections else { return }
        let all: [SectionInfo?] = [nil] + sections.map { Optional($0) }
        let i = all.firstIndex { $0?.id == section?.id } ?? 0
        section = all[(i + (backwards ? all.count - 1 : 1)) % all.count]
    }

    /// Makes the senders of the selected conversations VIPs, or, when they
    /// all are already, stops. Your own addresses are left out.
    func toggleVIP() {
        let rows: [ThreadRow] = if isMultiSelection {
            rows(of: selectedThreads).rangeView.flatMap { page(offset: $0.lowerBound, limit: $0.count) }
        } else {
            cursorRow.map { [$0] } ?? []
        }
        let mine = Set(accounts.map { $0.account.address.lowercased() })
        var seen = Set<String>()
        let senders = rows.map(\.fromAddr).filter { !$0.isEmpty && !mine.contains($0.lowercased()) && seen.insert($0.lowercased()).inserted }
        guard !senders.isEmpty else { return }
        let current = senders.filter { (try? core.isVip(address: $0)) ?? false }
        let vip = current.count < senders.count
        let changed = vip ? senders.filter { !current.contains($0) } : senders
        do {
            try core.setVips(addresses: changed, vip: vip)
        } catch {
            return show(Toast(text: "\(error)", undo: nil))
        }
        let who = changed.count == 1 ? changed[0] : "\(changed.count) senders"
        let text = switch (vip, changed.count == 1) {
        case (true, true): "\(who) is now a VIP"
        case (true, false): "\(who) are now VIPs"
        case (false, true): "\(who) is no longer a VIP"
        case (false, false): "\(who) are no longer VIPs"
        }
        show(Toast(text: text, undo: .vips(changed, !vip)))
    }

    /// Leaves the mailing list behind the open or highlighted conversation,
    /// then archives it. A list that only offers a web page gets that page
    /// opened in the browser.
    func unsubscribe() {
        guard let thread = selectedThreadID ?? cursorRow?.threadId else { return }
        show(Toast(text: "Unsubscribing…", undo: nil))
        Task {
            do {
                switch try await core.unsubscribe(threadId: thread) {
                case .unsubscribed(let list):
                    act(.archive, label: "Unsubscribed from \(list) and archived", threadID: thread)
                case .emailed(_, let to):
                    act(.archive, label: "Sent an unsubscribe email to \(to) and archived", threadID: thread)
                case .openPage(let list, let url):
                    if let page = URL(string: url) { NSWorkspace.shared.open(page) }
                    act(.archive, label: "Opened \(list)'s unsubscribe page and archived", threadID: thread)
                case .notAList:
                    show(Toast(text: "This conversation has no unsubscribe link", undo: nil))
                }
            } catch {
                show(Toast(text: "Couldn't unsubscribe: \(error)", undo: nil))
            }
        }
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
            syncErrors[id] = nil
        case .syncFailed(let id, let message):
            syncing.remove(id)
            syncErrors[id] = message
        case .newMail(_, let threadID, let from, let subject, let vip):
            if NewMailBanners.current.shows(vip: vip) {
                notifier.post(threadID: threadID, from: from, subject: subject)
            }
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
        case .noReply(_, let threadID, let subject):
            show(Toast(text: "No reply yet: \(subject)", undo: nil))
            if NewMailBanners.current != .none {
                notifier.post(threadID: threadID, from: "No reply yet", subject: subject)
            }
        case .sent(_, let subject):
            show(Toast(text: "Sent: \(subject.isEmpty ? "(no subject)" : subject)", undo: nil))
        case .sendFailed(let message):
            show(Toast(text: "Not sent yet: \(message)", undo: nil))
        case .responsesFiled(let text, let tokens):
            show(Toast(text: text, undo: .action(tokens)))
        case .modelChanged(let id, let state):
            if let i = localModels.firstIndex(where: { $0.id == id }) { localModels[i].state = state }
            if case .ready = state, ai.provider == .local, ai.localModel == id { reloadAI() }
        case .calendarChanged:
            calendar.reloadCalendars()
            calendar.refresh()
            // List rows show invitation answers, which an answer or a sync changes.
            listChanged()
        case .calendarSynced:
            calendar.reloadCalendars()
        case .calendarOpFailed(let message, _):
            show(Toast(text: message, undo: nil))
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

    /// Label… for the conversation under the cursor: the label picker on
    /// Gmail, else the core's reason there are no labels to pick.
    func openLabels() {
        guard let row = cursorRow else { return }
        do {
            _ = try core.threadLabels(threadId: row.threadId)
            palette = .label
        } catch {
            notify("\(error)")
        }
    }

    /// Whether the conversation under the cursor is on an account with labels.
    var cursorHasLabels: Bool {
        accounts.first { $0.id == cursorRow?.accountId }?.account.hasLabels == true
    }

    /// Marks everything unread in a list read; the current list by default.
    func markAllRead(_ list: ListSource? = nil) {
        let target = list ?? source
        do {
            let tokens = try core.markAllRead(query: list == nil ? query : target.query)
            show(Toast(text: tokens.isEmpty ? "Nothing unread in \(target.title)" : "Marked all read in \(target.title)",
                       undo: tokens.isEmpty ? nil : .action(tokens)))
        } catch {
            show(Toast(text: "\(error)", undo: nil))
        }
    }

    /// Trash folders behind a list: one account's, or every account's.
    func trashFolders(_ list: ListSource) -> [MailboxInfo] {
        let boxes = accounts.flatMap(\.mailboxes)
        switch list {
        case .role("trash"): return boxes.filter { $0.role == "trash" }
        case .mailbox(let id, _): return boxes.filter { $0.id == id && $0.role == "trash" }
        default: return []
        }
    }

    /// Asks before emptying Trash: there is no undo.
    var confirmEmpty: ListSource?

    func emptyTrash(_ list: ListSource) {
        do {
            try core.emptyMailboxes(mailboxIds: trashFolders(list).map(\.id))
            show(Toast(text: "Trash emptied", undo: nil))
        } catch {
            show(Toast(text: "\(error)", undo: nil))
        }
    }

    func undo() {
        switch toast?.undo {
        case .action(let tokens):
            try? core.undo(tokens: tokens)
            show(Toast(text: "Undone", undo: nil))
        case .vips(let addresses, let vip):
            try? core.setVips(addresses: addresses, vip: vip)
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
        composeReminder = nil
        composeSchedule = nil
        compose = (try? core.newCompose()) ?? .blank
    }

    func reply(_ mode: ReplyMode, threadID: Int64? = nil) {
        guard let thread = threadID ?? selectedThreadID else { return }
        let offset = Int32(TimeZone.current.secondsFromGMT())
        Task {
            do {
                composeReminder = nil
                composeSchedule = nil
                compose = try await core.replyCompose(threadId: thread, mode: mode, utcOffset: offset)
            } catch {
                show(Toast(text: "\(error)", undo: nil))
            }
        }
    }

    /// Sends the message being written now (after the undo window) or at
    /// `at`, with the reminder chosen for it, if any.
    func sendCompose(at: Int64? = nil, label: String? = nil) {
        guard let draft = compose else { return }
        do {
            let token = try core.send(compose: draft, sendAt: at, remindAt: composeReminder?.at)
            var text = label.map { "Scheduled for \($0)" } ?? "Sending in 10 seconds"
            if let reminder = composeReminder { text += ", reminder \(reminder.label)" }
            compose = nil
            composeReminder = nil
            composeSchedule = nil
            requestListFocus()
            show(Toast(text: text, undo: .send(token)))
        } catch {
            show(Toast(text: "\(error)", undo: nil))
        }
    }

    /// A reply to everyone on the conversation proposing a time Find a time
    /// picked. It stays a draft until the user sends it.
    func replyProposing(thread: Int64, start: Int64, end: Int64) {
        let offset = Int32(TimeZone.current.secondsFromGMT())
        Task {
            do {
                composeReminder = nil
                composeSchedule = nil
                compose = try await core.replyProposing(threadId: thread, start: start, end: end, utcOffset: offset)
            } catch {
                show(Toast(text: "\(error)", undo: nil))
            }
        }
    }

    /// Adds text after what was written in the message being written, above
    /// the signature and the quote.
    func insertIntoCompose(_ text: String) {
        guard let draft = compose, let body = try? core.addToWritten(body: draft.body, accountId: draft.accountId, text: text) else { return }
        compose?.body = body
    }

    /// Puts a snippet, filled in for the first recipient, at the cursor of
    /// the message being written.
    func insertSnippet(_ snippet: SnippetInfo) {
        guard let compose, let editor = RichTextView.messageBody else { return }
        editor.insert(body: core.expandSnippet(body: snippet.body, to: compose.to), core: core)
    }

    /// Opens a scheduled message for editing. It stays scheduled for the same
    /// time unless it is sent now, given a new time or turned into a draft.
    func editScheduled(_ message: ScheduledInfo) {
        // Replacing an open message would lose it, or its schedule.
        if compose != nil { closeCompose() }
        guard let back = try? core.cancelSend(token: message.token) else {
            return show(Toast(text: "Already sent", undo: nil))
        }
        composeReminder = nil
        composeSchedule = Reminder(at: message.sendAt, label: PaletteView.when(message.sendAt))
        compose = back
        reloadAccounts()
    }

    func saveDraft() {
        guard let draft = compose, !draft.isEmpty else { return }
        if let id = try? core.saveDraft(compose: draft), compose?.draftId == nil {
            compose?.draftId = id
        }
    }

    /// Closes the editor and keeps what was written as a draft, or, for a
    /// scheduled message, back in the outbox for its time.
    func closeCompose() {
        if let schedule = composeSchedule, compose?.isEmpty == false {
            let due = schedule.at <= Int64(Date.now.timeIntervalSince1970) + 10
            return due ? sendCompose() : sendCompose(at: schedule.at, label: schedule.label)
        }
        composeSchedule = nil
        saveDraft()
        if compose?.isEmpty == false { show(Toast(text: "Saved to Drafts", undo: nil)) }
        compose = nil
        requestListFocus()
    }

    func discardCompose() {
        composeSchedule = nil
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

    /// Saves a message's attachments into a folder the user picks, Downloads
    /// first, and shows them in the Finder.
    func saveAttachments(message: Int64) {
        let panel = NSOpenPanel()
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.canCreateDirectories = true
        panel.prompt = "Save Here"
        panel.message = "Choose a folder for the attachments."
        panel.directoryURL = FileManager.default.urls(for: .downloadsDirectory, in: .userDomainMask).first
        guard panel.runModal() == .OK, let dir = panel.url else { return }
        Task {
            do {
                let saved = try await core.saveAttachments(messageId: message, directory: dir.path)
                show(Toast(text: "Saved \(saved.count) attachment\(saved.count == 1 ? "" : "s") to \(dir.lastPathComponent)", undo: nil))
                NSWorkspace.shared.activateFileViewerSelecting(saved.map { URL(fileURLWithPath: $0) })
            } catch {
                show(Toast(text: "\(error)", undo: nil))
            }
        }
    }

    /// A message with nothing to undo.
    func notify(_ text: String) {
        show(Toast(text: text, undo: nil))
    }

    private func show(_ toast: Toast) {
        self.toast = toast
        let id = toast.id
        Task {
            try? await Task.sleep(for: .seconds(toast.review != nil ? 12 : toast.undo == nil ? 3 : 6))
            if self.toast?.id == id { self.toast = nil }
        }
    }

    /// `-JoltaTiming YES` appends launch and open timings to timing.txt in the data folder.
    static func timing(_ line: String) {
        guard UserDefaults.standard.bool(forKey: "JoltaTiming") else { return }
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
            summary?.refresh(label: ai.label)
        }
    }

    /// An empty list has nothing under the cursor, so the reader goes blank.
    func closeThread() {
        markReadTask?.cancel()
        selectedThreadID = nil
        selectedThreads = []
        openThread = nil
        showSummary(for: nil)
        cursorRow = nil
        cursor = (0, 0)
    }

    func open(threadID: Int64) {
        if selectedThreadID != threadID {
            answerChoices = AnswerChoices()
            rsvpPanel = nil
            rsvpMessage = nil
            expandedMessages = [:]
        }
        selectedThreadID = threadID
        selectedThreads = [threadID]
        showSummary(for: threadID)
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
                Self.timing("open thread \(threadID) failed: \(error)")
            }
        }
    }
}

struct Toast: Equatable {
    enum Undo: Equatable {
        case action([Int64])
        case send(Int64)
        /// Set these senders' VIP mark back to this.
        case vips([String], Bool)
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

// MARK: - AI

extension AppModel {
    func reloadAI() {
        if let settings = try? core.aiSettings() { ai = settings }
        localModels = core.localModels()
    }

    func saveAI(_ change: (inout AiSettingsInfo) -> Void) {
        var settings = ai
        change(&settings)
        try? core.setAiSettings(settings: settings)
        reloadAI()
    }

    /// The open conversation's summary: a saved one at once, a new one for
    /// long conversations once the cursor rests on it for a moment.
    fileprivate func showSummary(for threadID: Int64?) {
        guard summary?.threadID != threadID else { return }
        summaryTask?.cancel()
        summary?.cancel()
        guard let threadID else { summary = nil; return }
        let summary = ConversationSummary(threadID: threadID, core: core)
        self.summary = summary
        guard ai.provider != nil else { return }
        summaryTask = Task {
            try? await Task.sleep(for: .milliseconds(400))
            guard !Task.isCancelled, self.summary === summary else { return }
            summary.start(automatic: true, label: ai.label)
        }
    }

    /// ⌘J in the reader: summarise the open conversation, or again.
    func summarizeOpenThread() {
        guard let threadID = openThread?.threadId else { return }
        if summary?.threadID != threadID { showSummary(for: threadID) }
        guard let summary else { return }
        summary.start(automatic: false, fresh: summary.state == .done && !summary.outdated, label: ai.label)
    }

    func openSettings(_ page: SettingsPage) {
        settingsRequest = page
    }

    /// The search text reads like a request ("invoices from Els last month")
    /// rather than a query, so ⌘J would help.
    var searchLooksLikeRequest: Bool {
        let text = search.trimmingCharacters(in: .whitespaces)
        return ai.provider != nil && !text.contains(":") && text.split(separator: " ").count >= 2
    }

    /// ⌘J in the search field: the model turns the request into a query,
    /// which replaces the text, so it can be read and edited, and searches
    /// all mail.
    func searchWithAI() {
        let request = search.trimmingCharacters(in: .whitespaces)
        guard !request.isEmpty, !aiSearching else { return }
        guard ai.provider != nil else {
            show(Toast(text: "Searching with AI needs a model. Choose one in Settings › AI.", undo: nil))
            return openSettings(.ai)
        }
        aiSearching = true
        let offset = Int32(TimeZone.current.secondsFromGMT())
        Task {
            defer { aiSearching = false }
            do {
                guard let query = try await core.aiSearchQuery(request: request, utcOffset: offset),
                      search.trimmingCharacters(in: .whitespaces) == request else { return }
                source = .allMail
                search = query
            } catch {
                show(Toast(text: "\(error)", undo: nil))
            }
        }
    }
}
