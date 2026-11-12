@preconcurrency import JoltaFFI
import SwiftUI
import UserNotifications

/// One page of the settings window.
enum SettingsPage: Hashable {
    case general, appearance, account(Int64), calendar, sections, vips, snippets, ai, agents, activity, about
}

/// Where the keyboard is in the settings window: the search field, the
/// list of pages, or the open page's own arrow-key rows (Appearance, AI).
enum SettingsFocus: Hashable { case search, pages, page }

/// The settings window: pages in a searchable sidebar on the left, the
/// open page on the right as titled sections of rows. ↑ and ↓ walk through
/// the pages, or through a search's matches, where Return opens the one
/// marked.
struct SettingsView: View {
    let model: AppModel?
    @State private var page = SettingsPage.general
    @State private var search = ""
    @FocusState private var focus: SettingsFocus?
    /// The search match Return opens.
    @State private var marked = 0
    @State private var addingIMAP = false
    @State private var status: String?

    private struct Item {
        let page: SettingsPage
        let title: String
        let icon: String
        /// Words on the page, so search finds the page that holds a setting.
        let keywords: String
    }

    private var groups: [(title: String?, items: [Item])] {
        let all = model?.accounts ?? []
        let accounts = all.map { a in
            Item(page: .account(a.id), title: a.title, icon: Self.icon(a.account.provider),
                 keywords: "\(a.account.address) \(a.account.provider) account signature server imap smtp password folders sign in remove send mail as alias aliases from default sender new messages")
        }
        return [
            (nil, [
                Item(page: .general, title: "General", icon: "gearshape",
                     keywords: "notifications banners new mail vip contacts address suggestions autocomplete"),
                Item(page: .appearance, title: "Appearance", icon: "paintpalette",
                     keywords: "theme themes dark light mode macos graphite omarchy colors always match"),
            ]),
            ("Accounts", accounts),
            ("Calendar", [Item(page: .calendar, title: "Calendar", icon: "calendar",
                               keywords: "calendars google microsoft caldav icloud subscription webcal feed holidays color busy reminders notifications invitations responses replies file archive")]),
            ("Inbox", [
                Item(page: .sections, title: "Sections", icon: "rectangle.split.3x1", keywords: "split inbox tabs query newsletters notifications"),
                Item(page: .vips, title: "VIPs", icon: "checkmark.seal", keywords: "vip important senders people"),
            ]),
            ("Writing", [Item(page: .snippets, title: "Snippets", icon: "text.badge.plus", keywords: "templates canned replies text")]),
            ("Intelligence", [Item(page: .ai, title: "AI", icon: "sparkles",
                                   keywords: "summary summaries write draft model gemma local openai anthropic claude gpt api key download")]),
            ("Agents", [
                Item(page: .agents, title: "Agents", icon: "cpu", keywords: "mcp claude pause grants revoke connect"),
                Item(page: .activity, title: "Activity", icon: "list.bullet.rectangle", keywords: "audit log history"),
            ]),
            (nil, [Item(page: .about, title: "About", icon: "info.circle", keywords: "version updates sparkle data folder")]),
        ]
    }

    private var title: String {
        groups.flatMap(\.items).first { $0.page == page }?.title ?? "General"
    }

    static func icon(_ provider: String) -> String {
        switch provider {
        case "Google": "g.circle"
        case "Microsoft 365": "building.2"
        case "iCloud": "icloud"
        default: "envelope"
        }
    }

    var body: some View {
        HStack(spacing: 0) {
            sidebar
                .frame(width: 236)
                .background(Color(Theme.side))
                .overlay(alignment: .trailing) { Rectangle().fill(Color(Theme.line2)).frame(width: 1) }
            VStack(spacing: 0) {
                HStack(spacing: 8) {
                    Text("Settings").foregroundStyle(Color(Theme.text3))
                    Text("/").foregroundStyle(Color(Theme.text3))
                    Text(title).foregroundStyle(Color(Theme.text)).lineLimit(1)
                    Spacer()
                }
                .font(.system(size: Theme.title))
                .padding(.horizontal, 28)
                .frame(height: Theme.headerHeight)
                .background(TitleBarArea())
                .overlay(alignment: .bottom) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
                content.frame(maxWidth: .infinity, maxHeight: .infinity)
            }
            .background(Color(Theme.bg))
        }
        .foregroundStyle(Color(Theme.text))
        .font(.system(size: 13))
        .ignoresSafeArea()
        .frame(minWidth: 860, minHeight: 560)
        .background { Button("") { focus = .search }.keyboardShortcut("f").hidden() }
        .onAppear { show(model?.settingsRequest ?? page) }
        .onChange(of: model?.settingsRequest) { takeRequest() }
        .sheet(isPresented: $addingIMAP) {
            AddIMAPAccount(model: model) { added in
                addingIMAP = false
                if let added {
                    status = "Added \(added.address). Jolta is syncing it now."
                    show(.account(added.id))
                }
            }
        }
    }

    /// Shows the page another window asked for.
    private func takeRequest() {
        if let request = model?.settingsRequest { show(request) }
    }

    /// Opens a page and gives the sidebar the keyboard, so ↑ and ↓ move on
    /// from it. Appearance and AI answer the arrow keys themselves and get
    /// the keyboard as they open, unless the arrow keys are walking past.
    /// Return in the search field (`searching`) otherwise leaves the keyboard there.
    private func show(_ next: SettingsPage, walking: Bool = false, searching: Bool = false) {
        page = next
        model?.settingsRequest = nil
        let target: SettingsFocus? = walking ? .pages : [.appearance, .ai].contains(next) ? .page : searching ? nil : .pages
        // On the next turn, when the page is in the window.
        if let target { DispatchQueue.main.async { focus = target } }
    }

    /// ↑ and ↓: the next page, or the next match while a search shows.
    private func step(_ delta: Int) {
        if search.isEmpty {
            let pages = groups.flatMap(\.items).map(\.page)
            guard let i = pages.firstIndex(of: page) else { return }
            show(pages[min(max(i + delta, 0), pages.count - 1)], walking: true)
        } else {
            marked = min(max(marked + delta, 0), max(filtered.count - 1, 0))
        }
    }

    private func openMarked(walking: Bool = false) {
        let matches = filtered
        if matches.indices.contains(marked) { show(matches[marked].page, walking: walking, searching: true) }
    }

    @ViewBuilder private var content: some View {
        if let model {
            switch page {
            case .general: GeneralSettings(model: model)
            case .appearance: AppearanceSettings(focus: $focus)
            case .account(let id):
                if let section = model.accounts.first(where: { $0.id == id }) {
                    AccountPage(model: model, section: section) { show(.general) }.id(id)
                } else {
                    SettingsScroll { Text("This account was removed.").foregroundStyle(Color(Theme.text3)) }
                }
            case .calendar: CalendarSettings(model: model)
            case .sections: SectionsSettings(model: model)
            case .vips: VIPSettings(model: model)
            case .snippets: SnippetsSettings(model: model)
            case .ai: AISettings(model: model, focus: $focus)
            case .agents: AgentsSettings(model: model)
            case .activity: ActivitySettings(model: model)
            case .about: AboutSettings(model: model)
            }
        } else {
            ProgressView()
        }
    }

    private var sidebar: some View {
        VStack(alignment: .leading, spacing: 0) {
            // Room for the window's traffic lights.
            TitleBarArea().frame(height: 40)
            HStack(spacing: 8) {
                Image(systemName: "magnifyingglass").font(.system(size: 12)).foregroundStyle(Color(Theme.text3))
                TextField("Search", text: $search)
                    .textFieldStyle(.plain)
                    .focused($focus, equals: .search)
                    .onSubmit { openMarked() }
                    .onExitCommand { search = ""; focus = .pages }
                    .onKeyPress(.downArrow) { step(1); return .handled }
                    .onKeyPress(.upArrow) { step(-1); return .handled }
                    .onChange(of: search) { marked = 0 }
                if focus != .search && search.isEmpty { KeyCap(key: "⌘F") }
            }
            .fieldBox(focused: focus == .search)
            .padding(.horizontal, 12)
            .padding(.bottom, 6)
            ScrollView {
                VStack(alignment: .leading, spacing: 1) {
                    if search.isEmpty {
                        ForEach(Array(groups.enumerated()), id: \.offset) { _, group in
                            if let title = group.title { GroupLabel(title) } else { Spacer().frame(height: 10) }
                            ForEach(group.items, id: \.page) { row($0) }
                            if group.title == "Accounts" {
                                addAccount
                                if let model, model.signIn != nil {
                                    SignInStatusRow(model: model).padding(.horizontal, 10).padding(.vertical, 4)
                                }
                            }
                        }
                    } else {
                        Spacer().frame(height: 8)
                        ForEach(Array(filtered.enumerated()), id: \.element.page) { i, item in
                            NavRow(icon: item.icon, title: item.title, count: 0, selected: i == marked, marked: true) {
                                marked = i
                                show(item.page)
                            }
                        }
                        if filtered.isEmpty {
                            Text("No settings match “\(search)”")
                                .font(.system(size: 12))
                                .foregroundStyle(Color(Theme.text3))
                                .padding(10)
                        }
                    }
                }
                .padding(.horizontal, 10)
                .padding(.bottom, 12)
            }
            .scrollIndicators(.never)
            .focusable()
            .focused($focus, equals: .pages)
            .focusEffectDisabled()
            .onKeyPress(keys: [.upArrow, .downArrow]) { press in
                step(press.key == .upArrow ? -1 : 1)
                if !search.isEmpty { openMarked(walking: true) }
                return .handled
            }
            if let status {
                Text(status)
                    .font(.system(size: 11))
                    .foregroundStyle(Color(Theme.text3))
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(14)
            }
        }
    }

    private var filtered: [Item] {
        let needle = search.lowercased().trimmingCharacters(in: .whitespaces)
        return groups.flatMap(\.items).filter { "\($0.title) \($0.keywords)".lowercased().contains(needle) }
    }

    /// The open page's row carries the caret while the arrow keys move it.
    private func row(_ item: Item) -> some View {
        NavRow(icon: item.icon, title: item.title, count: 0, selected: page == item.page, marked: focus == .pages) { show(item.page) }
    }

    private var addAccount: some View {
        let signingIn = model?.signIn == .waiting
        return Menu {
            ForEach(NewAccount.allCases) { kind in
                Button("\(kind.name) Account…") { add(kind) }
            }
        } label: {
            HStack(spacing: 10) {
                Image(systemName: signingIn ? "hourglass" : "plus").font(.system(size: 13)).frame(width: 16)
                Text(signingIn ? "Signing in…" : "Add Account")
            }
            .foregroundStyle(Color(Theme.text3))
        }
        .menuStyle(.button)
        .buttonStyle(.plain)
        .menuIndicator(.hidden)
        .disabled(signingIn)
        .padding(.leading, 10)
        .frame(height: 28)
    }

    /// The same sign-in as the welcome screen's; here the new account's
    /// page opens, and the IMAP sheet lies over this window.
    private func add(_ kind: NewAccount) {
        status = nil
        switch kind {
        case .google: model?.signIn(.google) { show(.account($0.id)) }
        case .microsoft: model?.signIn(.microsoft) { show(.account($0.id)) }
        case .imap: addingIMAP = true
        }
    }
}

// MARK: - Building blocks

/// A settings page's scrolling column, centred at a readable width.
struct SettingsScroll<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 30) { content }
                .frame(maxWidth: 680, alignment: .leading)
                .padding(.horizontal, 32)
                .padding(.vertical, 28)
                .frame(maxWidth: .infinity)
        }
    }
}

/// A heading over one card of rows, with optional buttons beside the
/// heading and a note under the card.
struct SettingsSection<Content: View, Accessory: View>: View {
    let title: String
    var footer: String?
    @ViewBuilder let accessory: Accessory
    @ViewBuilder let content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .firstTextBaseline) {
                Text(title).font(.system(size: 15, weight: .semibold)).tracking(-0.2)
                Spacer()
                accessory
            }
            .padding(.horizontal, 4)
            SettingsCard { content }
            if let footer {
                Text(footer)
                    .font(.system(size: Theme.small))
                    .foregroundStyle(Color(Theme.text3))
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.horizontal, 4)
            }
        }
    }
}

extension SettingsSection where Accessory == EmptyView {
    init(_ title: String, footer: String? = nil, @ViewBuilder content: () -> Content) {
        self.init(title: title, footer: footer, accessory: { EmptyView() }, content: content)
    }
}

extension SettingsSection {
    init(_ title: String, footer: String? = nil, @ViewBuilder content: () -> Content, @ViewBuilder accessory: () -> Accessory) {
        self.init(title: title, footer: footer, accessory: accessory, content: content)
    }
}

/// One of a few choices side by side, as a card; the chosen one is filled.
struct ChoiceCard: View {
    let title: String
    let detail: String
    let icon: String
    let selected: Bool
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            VStack(alignment: .leading, spacing: 6) {
                Image(systemName: icon).font(.system(size: 14))
                Text(title).font(.system(size: Theme.body, weight: .semibold))
                Text(detail).font(.system(size: Theme.small)).foregroundStyle(Color(Theme.text3))
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(10)
            .background(RoundedRectangle(cornerRadius: 8).fill(Color(selected ? Theme.fill : Theme.bg)))
            .overlay(RoundedRectangle(cornerRadius: 8)
                .strokeBorder(Color(selected ? Theme.text3 : Theme.line), lineWidth: selected ? 1.5 : 1))
            .contentShape(RoundedRectangle(cornerRadius: 8))
        }
        .buttonStyle(.plain)
        .accessibilityAddTraits(selected ? .isSelected : [])
    }
}

/// Rows on one rounded card, with a hairline between them.
struct SettingsCard<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        VStack(spacing: 0) {
            Group(subviews: content) { rows in
                ForEach(rows) { row in
                    if row.id != rows.first?.id {
                        Rectangle().fill(Color(Theme.line)).frame(height: 1)
                    }
                    row
                }
            }
        }
        .card()
    }
}

/// One setting: what it is on the left, its control on the right.
struct SettingsRow<Control: View>: View {
    let title: String
    var detail: String?
    var detailColor = Color(Theme.text3)
    @ViewBuilder let control: Control

    var body: some View {
        HStack(spacing: 20) {
            VStack(alignment: .leading, spacing: 3) {
                Text(title).font(.system(size: 13, weight: .medium))
                if let detail {
                    Text(detail)
                        .font(.system(size: 12))
                        .foregroundStyle(detailColor)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            Spacer(minLength: 12)
            control
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 11)
        .frame(minHeight: 48)
    }
}

extension SettingsRow where Control == EmptyView {
    init(_ title: String, detail: String? = nil) {
        self.init(title: title, detail: detail) { EmptyView() }
    }
}

/// A fact that can't be changed here: its name on the left, the value on the right.
struct SettingsValueRow: View {
    let title: String
    let value: String

    var body: some View {
        SettingsRow(title: title) {
            Text(value).foregroundStyle(Color(Theme.text2)).lineLimit(1).truncationMode(.middle).textSelection(.enabled)
        }
    }
}

/// A row with free-form content, such as an editor or a list item.
struct SettingsBlock<Content: View>: View {
    @ViewBuilder let content: Content

    var body: some View {
        content
            .padding(.horizontal, 16)
            .padding(.vertical, 11)
            .frame(maxWidth: .infinity, alignment: .leading)
    }
}

extension View {
    /// A text field for the right side of a row.
    func settingsField(width: CGFloat? = 260, mono: Bool = false) -> some View {
        modifier(SettingsField(width: width, mono: mono))
    }

    /// The frame of a rich text editor inside a card.
    func settingsEditor(height: CGFloat) -> some View {
        SettingsEditor(height: height) { self }
    }

    /// A switch in the accent colour, label hidden (the row names it).
    func settingsSwitch() -> some View {
        self.toggleStyle(.switch).labelsHidden().controlSize(.small).tint(Color(Theme.accent))
    }

    /// A pop-up menu sized to its choice, label hidden.
    func settingsMenu() -> some View {
        self.pickerStyle(.menu).labelsHidden().fixedSize()
    }
}

private struct SettingsField: ViewModifier {
    let width: CGFloat?
    let mono: Bool
    @FocusState private var focused: Bool

    func body(content: Content) -> some View {
        content
            .textFieldStyle(.plain)
            .font(.system(size: Theme.body, design: mono ? .monospaced : .default))
            .focused($focused)
            .fieldBox(focused: focused, width: width, fill: Theme.bg)
    }
}

private struct SettingsEditor<Content: View>: View {
    let height: CGFloat
    @ViewBuilder let content: Content
    @Environment(\.controlActiveState) private var window

    var body: some View {
        // The editor is an AppKit view, outside SwiftUI's focus. A settings
        // page shows one at most, so the key window's editor is this one.
        content
            .padding(.horizontal, 2)
            .padding(.vertical, 8)
            .fieldBox(focused: window == .key && FormatState.shared.editing, height: height, fill: Theme.bg)
    }
}

// MARK: - General and About

struct GeneralSettings: View {
    let model: AppModel
    @AppStorage(AddressBook.key) private var contacts = false
    @State private var contactsRefused = false
    /// Whether macOS offers Time Sensitive for Jolta: only builds signed with its entitlement.
    @State private var timeSensitive = false

    private var notificationsFooter: String {
        let about = "Important mail is mail from your VIPs, replies in conversations you wrote in, mail sent to you by people you've written to, and invitations. With AI sorting on, it also includes mail that asks you for a reply. Several messages at once arrive as one notification."
        return timeSensitive ? about + " To let urgent mail through a Focus, allow Time Sensitive notifications for Jolta in System Settings." : about
    }

    var body: some View {
        SettingsScroll {
            SettingsSection("Notifications", footer: notificationsFooter) {
                SettingsRow(title: "Notify me about", detail: "Which new inbox mail gets a notification while Jolta runs.") {
                    Picker("Notify me about", selection: Binding(get: { model.notifySettings.scope }, set: { model.notifySettings.scope = $0 })) {
                        Text("Important mail").tag(NotifyScope.important)
                        Text("All new mail").tag(NotifyScope.allNewMail)
                        Text("Only VIPs").tag(NotifyScope.vips)
                        Text("None").tag(NotifyScope.nothing)
                    }
                    .settingsMenu()
                    .accessibilityIdentifier("notify-scope")
                }
                SettingsRow(title: "Play a sound", detail: "At most once a minute. A notification that bundles several messages is silent.") {
                    Toggle("Play a sound", isOn: Binding(get: { model.notifySettings.sound }, set: { model.notifySettings.sound = $0 }))
                        .settingsSwitch()
                        .accessibilityIdentifier("notify-sound")
                }
                SettingsRow(title: "Dock badge", detail: "Counts unread conversations in your inboxes. The menu bar shows the same number.") {
                    Picker("Dock badge", selection: Binding(get: { model.notifySettings.badge }, set: { model.notifySettings.badge = $0 })) {
                        Text("All unread").tag(BadgeScope.allUnread)
                        Text("Important unread").tag(BadgeScope.importantUnread)
                        Text("None").tag(BadgeScope.nothing)
                    }
                    .settingsMenu()
                    .accessibilityIdentifier("dock-badge")
                }
            }
            SettingsSection("Writing", footer: "Jolta suggests people you've written to or replied to as you type in To, Cc and Bcc, the most frequent and recent first. Hover over a suggestion and click × to stop suggesting it.") {
                DefaultSenderRow(model: model)
                SettingsRow(title: "Suggest people from Contacts",
                            detail: contactsRefused
                                ? "Jolta can't read Contacts. Allow it in System Settings › Privacy & Security › Contacts."
                                : "Adds your Mac's contacts to the suggestions. They stay on this Mac.",
                            detailColor: contactsRefused ? Color(Theme.dangerText) : Color(Theme.text3)) {
                    Toggle("Suggest people from Contacts", isOn: $contacts)
                        .settingsSwitch()
                        .onChange(of: contacts) { _, on in toggleContacts(on) }
                }
            }
        }
        .task { timeSensitive = await UNUserNotificationCenter.current().notificationSettings().timeSensitiveSetting != .notSupported }
    }

    private func toggleContacts(_ on: Bool) {
        guard on else { return AddressBook.load(into: model.core) }
        contactsRefused = false
        Task {
            if await AddressBook.requestAccess() {
                AddressBook.load(into: model.core)
            } else {
                contactsRefused = true
                contacts = false
            }
        }
    }
}

struct AboutSettings: View {
    let model: AppModel
    let updater = Updater.shared

    private var version: String {
        let info = Bundle.main.infoDictionary
        return "\(info?["CFBundleShortVersionString"] as? String ?? "?") (\(info?["CFBundleVersion"] as? String ?? "?"))"
    }

    var body: some View {
        SettingsScroll {
            SettingsSection("Jolta") {
                SettingsValueRow(title: "Version", value: version)
                SettingsRow(title: "Updates", detail: "Jolta checks for a new version every hour and asks before installing it.") {
                    Button("Check for Updates…") { updater.check() }.disabled(!updater.canCheck)
                }
                SettingsRow(title: "Data folder", detail: AppModel.dataDirectory) {
                    Button("Show in Finder") {
                        NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: AppModel.dataDirectory)])
                    }
                }
            }
        }
    }
}

/// ⌘, opens the settings window.
struct SettingsMenuButton: View {
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Button("Settings…") {
            openWindow(id: "settings")
            NSApp.activate()
        }
        .keyboardShortcut(",")
    }
}
