@preconcurrency import JoltaFFI
import SwiftUI

/// One page of the settings window.
enum SettingsPage: Hashable {
    case general, account(Int64), calendar, sections, vips, snippets, ai, agents, activity, about
}

/// The settings window: pages in a searchable sidebar on the left, the
/// open page on the right as titled sections of rows.
struct SettingsView: View {
    let model: AppModel?
    @State private var page = SettingsPage.general
    @State private var search = ""
    @FocusState private var searching: Bool
    @State private var addingIMAP = false
    @State private var signingIn = false
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
                 keywords: "\(a.account.address) \(a.account.provider) account signature server imap smtp password folders sign in remove")
        }
        return [
            (nil, [Item(page: .general, title: "General", icon: "gearshape",
                        keywords: "notifications banners new mail vip contacts address suggestions autocomplete")]),
            ("Accounts", accounts),
            ("Calendar", [Item(page: .calendar, title: "Calendar", icon: "calendar",
                               keywords: "calendars google microsoft caldav icloud subscription webcal feed holidays color busy reminders notifications")]),
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
                .font(.system(size: 14))
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
        .background { Button("") { searching = true }.keyboardShortcut("f").hidden() }
        .onAppear(perform: takeRequest)
        .onChange(of: model?.settingsRequest) { takeRequest() }
        .sheet(isPresented: $addingIMAP) {
            AddIMAPAccount(model: model) { added in
                addingIMAP = false
                if let added {
                    status = "Added \(added.address). Jolta is syncing it now."
                    page = .account(added.id)
                }
            }
        }
    }

    /// Shows the page another window asked for.
    private func takeRequest() {
        guard let request = model?.settingsRequest else { return }
        page = request
        model?.settingsRequest = nil
    }

    @ViewBuilder private var content: some View {
        if let model {
            switch page {
            case .general: GeneralSettings(model: model)
            case .account(let id):
                if let section = model.accounts.first(where: { $0.id == id }) {
                    AccountPage(model: model, section: section) { page = .general }.id(id)
                } else {
                    SettingsScroll { Text("This account was removed.").foregroundStyle(Color(Theme.text3)) }
                }
            case .calendar: CalendarSettings(model: model)
            case .sections: SectionsSettings(model: model)
            case .vips: VIPSettings(model: model)
            case .snippets: SnippetsSettings(model: model)
            case .ai: AISettings(model: model)
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
                    .focused($searching)
                    .onSubmit { if let first = filtered.first { page = first.page } }
                    .onExitCommand { search = ""; searching = false }
                if !searching && search.isEmpty { KeyCap(key: "⌘F") }
            }
            .padding(.horizontal, 10)
            .frame(height: 30)
            .background(RoundedRectangle(cornerRadius: 8).fill(Color(Theme.fill)))
            .padding(.horizontal, 12)
            .padding(.bottom, 6)
            ScrollView {
                VStack(alignment: .leading, spacing: 1) {
                    if search.isEmpty {
                        ForEach(Array(groups.enumerated()), id: \.offset) { _, group in
                            if let title = group.title { GroupLabel(title) } else { Spacer().frame(height: 10) }
                            ForEach(group.items, id: \.page) { row($0) }
                            if group.title == "Accounts" { addAccount }
                        }
                    } else {
                        Spacer().frame(height: 8)
                        ForEach(filtered, id: \.page) { row($0) }
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

    private func row(_ item: Item) -> some View {
        NavRow(icon: item.icon, title: item.title, count: 0, selected: page == item.page) { page = item.page }
    }

    private var addAccount: some View {
        Menu {
            Button("Google Account…") { signIn(.google) }
            Button("Microsoft 365 Account…") { signIn(.microsoft) }
            Button("iCloud or IMAP Account…") { addingIMAP = true }
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

    private func signIn(_ provider: OAuthProvider) {
        guard let core = model?.core else { return }
        signingIn = true
        status = "Continue in your browser…"
        Task {
            do {
                let session = try await core.beginSignIn(provider: provider)
                if let url = URL(string: session.url()) { NSWorkspace.shared.open(url) }
                let account = try await session.finish()
                status = "Signed in as \(account.address)."
                model?.reloadAccounts()
                page = .account(account.id)
            } catch {
                status = "Sign-in failed: \(error)"
            }
            signingIn = false
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
                    .font(.system(size: 11.5))
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
                Text(title).font(.system(size: 12.5, weight: .semibold))
                Text(detail).font(.system(size: 11.5)).foregroundStyle(Color(Theme.text3))
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
                        Rectangle().fill(Color(Theme.line2)).frame(height: 1)
                    }
                    row
                }
            }
        }
        .background(RoundedRectangle(cornerRadius: 10).fill(Color(Theme.raise)))
        .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Color(Theme.line), lineWidth: 1))
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
    /// A text field drawn the Graphite way, for the right side of a row.
    func settingsField(width: CGFloat? = 260, mono: Bool = false) -> some View {
        self
            .textFieldStyle(.plain)
            .font(.system(size: 12.5, design: mono ? .monospaced : .default))
            .padding(.horizontal, 8)
            .frame(width: width, height: 26)
            .background(RoundedRectangle(cornerRadius: 6).fill(Color(Theme.bg)))
            .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color(Theme.line), lineWidth: 1))
    }

    /// The frame of a multi-line editor inside a card.
    func settingsEditor(height: CGFloat) -> some View {
        self
            .padding(.horizontal, 10)
            .padding(.vertical, 8)
            .frame(height: height)
            .background(RoundedRectangle(cornerRadius: 6).fill(Color(Theme.bg)))
            .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color(Theme.line), lineWidth: 1))
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

// MARK: - General and About

struct GeneralSettings: View {
    let model: AppModel
    @AppStorage("NewMailBanners") private var banners = NewMailBanners.all
    @AppStorage(AddressBook.key) private var contacts = false
    @State private var contactsRefused = false

    var body: some View {
        SettingsScroll {
            SettingsSection("Notifications") {
                SettingsRow(title: "New mail banners", detail: "Which new inbox mail gets a macOS banner while Jolta runs.") {
                    Picker("New mail banners", selection: $banners) {
                        Text("Every new message").tag(NewMailBanners.all)
                        Text("Only from VIPs").tag(NewMailBanners.vips)
                        Text("None").tag(NewMailBanners.none)
                    }
                    .settingsMenu()
                }
            }
            SettingsSection("Writing", footer: "Jolta suggests people you've written to or replied to as you type in To, Cc and Bcc, the most frequent and recent first. Hover over a suggestion and click × to stop suggesting it.") {
                SettingsRow(title: "Suggest people from Contacts",
                            detail: contactsRefused
                                ? "Jolta can't read Contacts. Allow it in System Settings › Privacy & Security › Contacts."
                                : "Adds your Mac's contacts to the suggestions. They stay on this Mac.",
                            detailColor: contactsRefused ? .red : Color(Theme.text3)) {
                    Toggle("Suggest people from Contacts", isOn: $contacts)
                        .settingsSwitch()
                        .onChange(of: contacts) { _, on in toggleContacts(on) }
                }
            }
        }
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
