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
        case .starred: "is:starred"
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
    /// Accounts whose automatic replies are on now, for the strip over the list.
    private(set) var away: [AwayInfo] = []
    private(set) var turningOffAway: Set<Int64> = []
    /// Counts `AwayChanged`, so Settings reloads states that aren't on now too.
    private(set) var awayRevision = 0
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
    /// The open conversation's holds line (C4 U10), and a count that
    /// changes whenever holds change, so the reader reads it again.
    var holdLine: HoldLine?
    var holdsRevision = 0
    /// The held time B books: the one picked on the line, else the first.
    var holdCursor: Int64?
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
    /// The Out of office sheet, while it is up over the main window.
    var awaySheet: AwaySheetRequest?
    /// The Focus time sheet, while it is up over the main window.
    var focusSheet: FocusSheetRequest?
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
    /// X from the palette: the message list picks or drops the row under its caret.
    @ObservationIgnored var togglePicked: () -> Void = {}
    /// The last action, shown briefly with its undo key.
    private(set) var toast: Toast?
    /// Each message opened in compose gets its own number, so the editor
    /// starts afresh, cursor included, even in place of another message.
    private(set) var composeSession = 0
    /// The message being written, shown in place of the reader.
    /// Compose lives in mail mode; starting one from calendar mode goes there.
    var compose: ComposeData? {
        didSet {
            if compose != nil, mode == .calendar { mode = .mail }
            // The reader skipped calendar changes while compose covered it.
            if compose == nil, oldValue != nil { refreshOpenThread() }
        }
    }
    /// The AI settings, and the models that can run on this Mac.
    private(set) var ai: AiSettingsInfo
    private(set) var localModels: [LocalModelInfo] = []
    /// The open conversation's summary.
    private(set) var summary: ConversationSummary?
    private var summaryTask: Task<Void, Never>?
    /// A settings page something asked to show; the window opens on it.
    var settingsRequest: SettingsPage?
    /// Goes up when an account's aliases change, so an open From menu lists them again.
    var sendersRevision = 0
    /// The search field has the keyboard, so ⌘J searches with AI.
    var searchFocused = false
    private(set) var aiSearching = false
    private var markReadTask: Task<Void, Never>?
    /// The user archived, moved or undid something, so the list's next
    /// change may move the cursor on to the next conversation.
    @ObservationIgnored var userChangedList = false
    /// The query of the list last shown, nil before the first. The list view
    /// is made again when the palette or calendar mode gives way, so a list
    /// switched to meanwhile is told apart from the same list shown again.
    @ObservationIgnored var shownListQuery: String?
    let notifier = Notifier()
    /// The Dock badge's number, which the menu bar item shows too.
    private(set) var badgeCount: UInt32 = 0
    /// A banner's Archive or Mark as Read clicked while Jolta was in the
    /// background: its toast, with Undo, shows when Jolta comes forward.
    private var noticeToast: Toast?
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

    /// The list is Drafts, where opening a conversation edits its draft.
    var showsDrafts: Bool {
        switch source {
        case .role(let role): role == "drafts"
        case .mailbox(let id, _): accounts.flatMap(\.mailboxes).first { $0.id == id }?.role == "drafts"
        default: false
        }
    }

    var query: String {
        var parts = [source.query]
        if showsSections, let section { parts.append(section.query) }
        return searchWithin(list: parts.filter { !$0.isEmpty }.joined(separator: " "), search: search.trimmingCharacters(in: .whitespaces))
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
        reloadAway()
        try core.start()
        Task { [weak self] in
            for await _ in NotificationCenter.default.notifications(named: NSApplication.didBecomeActiveNotification) {
                self?.core.checkAway()
                self?.core.setForeground(active: true)
                if let toast = self?.noticeToast {
                    self?.noticeToast = nil
                    self?.show(toast)
                }
            }
        }
        Task { [weak self] in
            for await _ in NotificationCenter.default.notifications(named: NSApplication.didResignActiveNotification) {
                self?.core.setForeground(active: false)
            }
        }
        core.setForeground(active: NSApp?.isActive ?? false)
        startNotifications()
        if !Self.isTestData { startMCP() }
        power.start(core: core)
        AddressBook.load(into: core)
        notifier.onOpen = { [weak self] threadID in
            self?.mode = .mail
            self?.source = .unifiedInbox
            if let threadID { self?.open(threadID: threadID) }
        }
        notifier.onReply = { [weak self] threadID in
            self?.mode = .mail
            self?.source = .unifiedInbox
            self?.open(threadID: threadID)
            self?.reply(.reply, threadID: threadID)
        }
        notifier.onAction = { [weak self] id, action in self?.noticeAction(id: id, action: action) }
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
        // `-JoltaSettings general|ai|calendar|appearance` opens the settings window on that page, for screenshots.
        switch defaults.string(forKey: "JoltaSettings") {
        case "general": settingsRequest = .general
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

    /// Closes the question without an answer. The agent stays waiting in
    /// Settings › Agents, where it can be allowed or revoked.
    func postponeAgent() {
        guard let agent = pendingAgent else { return }
        pendingAgent = nil
        show(Toast(text: "\(agent.name) is waiting in Settings › Agents", undo: nil))
    }

    func reviewDraft(_ id: Int64) {
        if let draft = try? core.draft(draftId: id) {
            openCompose(draft)
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

    /// Gives the keyboard back when a panel over the window closes: to the
    /// message being written, else to the list, where a typed letter is a
    /// command.
    func returnKeyboard() {
        if compose != nil, let body = RichTextView.messageBody {
            body.window?.makeFirstResponder(body)
        } else {
            requestListFocus()
        }
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
            // A sent copy or a reply changes the holds line.
            holdsRevision += 1
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
        case .notice(let notice):
            if case .badge(let count) = notice { badgeCount = count }
            notifier.deliver(notice)
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
        case .noReply(_, _, let subject):
            show(Toast(text: "No reply yet: \(subject)", undo: nil))
        case .sent(_, let subject):
            show(Toast(text: "Sent: \(subject.isEmpty ? "(no subject)" : subject)", undo: nil))
        case .sendFailed(let message, let gaveUp):
            show(Toast(text: "\(gaveUp ? "Not sent" : "Not sent yet"): \(message)", undo: nil))
        case .responsesFiled(let text, let tokens):
            show(Toast(text: text, undo: .action(tokens)))
        case .modelChanged(let id, let state):
            if let i = localModels.firstIndex(where: { $0.id == id }) { localModels[i].state = state }
            if case .ready = state, ai.provider == .local, ai.localModel == id { reloadAI() }
        case .calendarChanged:
            calendar.reloadCalendars()
            calendar.refresh()
            // List rows and invitation cards show the calendar: an answer, a
            // sync or an accepted proposal changes them.
            listChanged()
            refreshOpenThread()
            holdsRevision += 1
        case .calendarSynced:
            calendar.reloadCalendars()
        case .calendarOpFailed(let message, _):
            show(Toast(text: message, undo: nil))
        case .calendarNotice(let message):
            show(Toast(text: message, undo: nil))
        case .awayChanged:
            reloadAway()
        case .holdsChanged(_, let notice):
            holdsRevision += 1
            if let notice { showHolds(notice) }
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
    /// Settings › General › Notifications.
    var notifySettings: NotifySettings {
        get { _ = notifyRevision; return (try? core.notifySettings()) ?? NotifySettings(scope: .important, sound: true, badge: .importantUnread) }
        set {
            do { try core.setNotifySettings(settings: newValue) } catch { notify("\(error)") }
            notifyRevision += 1
        }
    }
    private var notifyRevision = 0

    /// The setting the app kept before the core had one moves over once;
    /// earlier notifications go, and the badge starts from the core's count.
    private func startNotifications() {
        let defaults = UserDefaults.standard
        if let old = defaults.string(forKey: "NewMailBanners") {
            _ = try? core.migrateNewMailBanners(old: old)
            defaults.removeObject(forKey: "NewMailBanners")
        }
        badgeCount = (try? core.badgeCount()) ?? 0
        notifier.deliver(.badge(count: badgeCount))
        Task { await notifier.clearEarlierNotices() }
    }

    private func noticeAction(id: String, action: NoticeAction) {
        do {
            let tokens = try core.noticeAction(id: id, action: action)
            guard !tokens.isEmpty else { return }
            let toast = Toast(text: action == .archive ? "Archived" : "Marked read", undo: .action(tokens))
            if NSApp.isActive { show(toast) } else { noticeToast = toast }
        } catch {
            noticeToast = Toast(text: "\(error)", undo: nil)
        }
    }

    func act(_ action: ThreadAction, label: String, threadID: Int64? = nil) {
        let threads = threadID.map { [$0] } ?? (isMultiSelection ? selectedThreads : selectedThreadID.map { [$0] } ?? [])
        guard !threads.isEmpty else { return }
        userChangedList = true
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

    /// What Z takes back: the last action that offered Undo. Kept apart from
    /// the toast, so the action can be undone after its message has left or
    /// another message has replaced it.
    private var lastUndo: Toast.Undo?
    /// The message Discard closed, for Undo to open again.
    private var discarded: ComposeData?

    func undo() {
        let undo = lastUndo
        lastUndo = nil
        switch undo {
        case .action(let tokens):
            userChangedList = true
            try? core.undo(tokens: tokens)
            show(Toast(text: "Undone", undo: nil))
        case .vips(let addresses, let vip):
            try? core.setVips(addresses: addresses, vip: vip)
            show(Toast(text: "Undone", undo: nil))
        case .calendar(let token):
            show(Toast(text: calendar.undo(token: token), undo: nil))
        case .holds(let setId, let ops):
            do {
                show(Toast(text: try core.undoHoldRelease(setId: setId, ops: ops), undo: nil))
            } catch {
                show(Toast(text: "\(error)", undo: nil))
            }
            holdsRevision += 1
        case .send(let token):
            if let back = try? core.cancelSend(token: token) {
                openCompose(back)
                show(Toast(text: "Sending cancelled", undo: nil))
            } else {
                show(Toast(text: "Already sent", undo: nil))
            }
        case .discard:
            guard var draft = discarded else { return }
            discarded = nil
            // The stored draft is gone; the next save stores it again.
            draft.draftId = nil
            openCompose(draft)
        case nil:
            show(Toast(text: "Nothing to undo", undo: nil))
        }
    }

    // MARK: Compose

    /// Opens a message in compose, in place of any other. The compose pane
    /// lives beside the mail list, so the calendar's messages open here too.
    func openCompose(_ message: ComposeData) {
        composeReminder = nil
        composeSchedule = nil
        compose = message
        composeSession += 1
    }

    /// A new message, from the account whose mailbox is in view.
    func startCompose() {
        let mailbox: Int64? = if case .mailbox(let id, _) = source { id } else { nil }
        openCompose((try? core.newCompose(mailboxId: mailbox)) ?? .blank)
    }

    /// Edits the conversation's draft in compose, or reads the conversation
    /// when its draft is being sent or discarded.
    func openDraft(threadID: Int64) {
        Task {
            do {
                guard let draft = try await core.openDraft(threadId: threadID) else { return open(threadID: threadID) }
                if compose?.draftId == draft.draftId { return }
                if compose != nil { closeCompose() }
                openCompose(draft)
            } catch {
                show(Toast(text: "\(error)", undo: nil))
            }
        }
    }

    func reply(_ mode: ReplyMode, threadID: Int64? = nil) {
        guard let thread = threadID ?? selectedThreadID else { return }
        let offset = Int32(TimeZone.current.secondsFromGMT())
        Task {
            do {
                openCompose(try await core.replyCompose(threadId: thread, mode: mode, utcOffset: offset))
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
            var text = label.map { "Scheduled for \($0)" } ?? "Sending in \(Toast.countdownMark)"
            if let reminder = composeReminder { text += ", reminder \(reminder.label)" }
            compose = nil
            composeReminder = nil
            composeSchedule = nil
            requestListFocus()
            show(Toast(text: text, undo: .send(token), until: label == nil ? Date.now + Toast.undoWindow : nil))
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
                openCompose(try await core.replyProposing(threadId: thread, start: start, end: end, utcOffset: offset))
            } catch {
                show(Toast(text: "\(error)", undo: nil))
            }
        }
    }

    /// Hold these times: one line per picked time goes into the message,
    /// and the draft holds them once it is sent.
    func insertHeldTimes(_ slots: [TimeSlot], account: Int64, zone: String?) {
        do {
            let text = try core.holdText(slots: slots, zone: zone)
            insertIntoCompose(text)
            saveDraft()
            guard let draft = compose?.draftId else { return notify("Write the message first, then insert the times") }
            try core.holdTimes(draftId: draft, accountId: account, slots: slots, zone: zone)
        } catch {
            notify("\(error)")
        }
    }

    /// A holds toast: Undo takes back a release, Release releases new holds.
    func showHolds(_ notice: HoldNotice) {
        show(Toast(text: notice.text, undo: notice.undo.isEmpty ? nil : .holds(notice.setId, notice.undo), release: notice.release ? notice.setId : nil))
    }

    /// Release all on the holds line, or Release on the toast.
    func releaseHolds(_ setId: String) {
        do {
            showHolds(try core.releaseHolds(setId: setId))
            holdsRevision += 1
        } catch {
            notify("\(error)")
        }
    }

    /// Book (B): the event editor at the held time with the recipients.
    /// Saving the meeting books it, and Jolta releases the other holds.
    func bookHold(_ slot: HoldSlotRow? = nil) {
        let held = holdLine?.slots.filter { $0.state == .held } ?? []
        guard let line = holdLine, let slot = slot ?? held.first(where: { $0.start == holdCursor }) ?? held.first else {
            return notify("No held time to book in this conversation")
        }
        do {
            openInvite(try core.bookHold(setId: line.setId, start: slot.start))
        } catch {
            notify("\(error)")
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
        openCompose(back)
        composeSchedule = Reminder(at: message.sendAt, label: PaletteView.when(message.sendAt))
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
        if let draft = compose, !draft.isEmpty {
            discarded = draft
            show(Toast(text: "Message discarded", undo: .discard))
        }
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
                let url = dir.appendingPathComponent(file.name)
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

    func reloadAway() {
        away = (try? core.awayStates()) ?? []
        awayRevision += 1
    }

    /// Turns automatic replies off, for an account whose replies Jolta can
    /// change (Microsoft 365).
    func turnOffAway(_ info: AwayInfo) {
        turningOffAway.insert(info.accountId)
        Task {
            do {
                show(Toast(text: try await core.turnOffAway(accountId: info.accountId), undo: nil))
            } catch {
                show(Toast(text: "Couldn't turn automatic replies off: \(error)", undo: nil))
            }
            turningOffAway.remove(info.accountId)
            reloadAway()
        }
    }

    func openAwaySettings(_ info: AwayInfo) {
        if let url = URL(string: info.settingsUrl) { NSWorkspace.shared.open(url) }
    }

    /// A calendar edit's message, which Undo takes back while it shows.
    func notify(_ text: String, calendarUndo token: Int64?) {
        show(Toast(text: text, undo: token.map { .calendar($0) }))
    }

    private func show(_ toast: Toast) {
        self.toast = toast
        if let undo = toast.undo { lastUndo = undo }
        let id = toast.id
        Task {
            try? await Task.sleep(for: .seconds(toast.seconds))
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
        /// A calendar edit's op.
        case calendar(Int64)
        /// Set these senders' VIP mark back to this.
        case vips([String], Bool)
        /// Bring back the holds of a set that these calendar ops release.
        case holds(String, [Int64])
        /// Open the message Discard closed.
        case discard
    }

    /// How long a send or a calendar change waits before it goes out, as
    /// the core's `UNDO_SEND_SECONDS`.
    static let undoWindow: TimeInterval = 10
    /// Where `text` shows the seconds left until `until`.
    static let countdownMark = "{seconds}"

    let id = UUID()
    let text: String
    let undo: Undo?
    /// A draft to open for review (an agent wrote it).
    var review: Int64?
    /// A hold set the toast's Release releases.
    var release: String?
    /// When the action goes out for good; the toast stays and counts down until then.
    var until: Date?

    /// How long the toast shows. One that offers Undo for something the core
    /// holds back stays for as long as the core does.
    var seconds: TimeInterval {
        if review != nil { return 12 }
        if let until { return max(1, until.timeIntervalSinceNow) }
        switch undo {
        case .calendar: return Self.undoWindow
        case .some: return 6
        case nil: return release == nil ? 3 : 6
        }
    }

    /// `text` with the seconds left filled in.
    func text(at now: Date) -> String {
        guard let until else { return text }
        let left = max(1, Int(until.timeIntervalSince(now).rounded(.up)))
        return text.replacingOccurrences(of: Self.countdownMark, with: left == 1 ? "1 second" : "\(left) seconds")
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
