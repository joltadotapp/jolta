@preconcurrency import JoltaFFI
import SwiftUI

/// One account's signature, saved a moment after each edit.
private struct SignatureEditor: View {
    let core: Core
    let accountID: Int64
    @State private var text = ""
    @State private var loaded = false
    @State private var saveTask: Task<Void, Never>?

    var body: some View {
        Group {
            if loaded {
                RichTextEditor(text: $text, core: core, fontSize: 13)
                    .settingsEditor(height: 96)
                    .onChange(of: text) { scheduleSave() }
            } else {
                Color.clear.frame(height: 96)
            }
        }
        .task(id: accountID) {
            saveTask?.cancel()
            text = (try? core.signature(accountId: accountID)) ?? ""
            loaded = true
        }
        .onDisappear { save() }
    }

    private func scheduleSave() {
        saveTask?.cancel()
        saveTask = Task {
            try? await Task.sleep(for: .milliseconds(600))
            guard !Task.isCancelled else { return }
            save()
        }
    }

    private func save() {
        guard loaded else { return }
        try? core.setSignature(accountId: accountID, signature: text)
    }
}

/// Settings › an account: its name and signature, its servers (IMAP) or
/// sign-in (Google, Microsoft), its folders, and removal.
struct AccountPage: View {
    let model: AppModel
    let section: AccountSection
    let removed: () -> Void
    @State private var saved: AccountSettingsInfo?
    @State private var draft: AccountSettingsInfo?
    @State private var password = ""
    @State private var smtpPassword = ""
    @State private var saving = false
    @State private var message: (text: String, failed: Bool)?
    @State private var removal: Confirmation?
    @State private var signingIn = false

    private var isIMAP: Bool { section.account.kind == "imap" }
    private var changed: Bool { draft != saved || !password.isEmpty || !smtpPassword.isEmpty }

    /// User folders: the inbox and the role folders always show.
    private var folders: [MailboxInfo] { section.mailboxes.filter { $0.role == nil } }

    var body: some View {
        if draft != nil {
            page
        } else {
            ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity).onAppear(perform: load)
        }
    }

    private var page: some View {
        SettingsScroll {
            SettingsSection("Account") {
                SettingsValueRow(title: "Provider", value: section.account.provider)
                SettingsRow(title: "Name", detail: "Shown as the sender of your mail.") {
                    TextField("", text: binding(\.displayName)).settingsField()
                }
                if isIMAP {
                    SettingsRow(title: "Email address") { TextField("", text: binding(\.address)).settingsField() }
                } else {
                    SettingsValueRow(title: "Email address", value: section.account.address)
                }
            }
            SendAsSection(model: model, account: section.account)
            SettingsSection("Signature", footer: "Added below new messages, replies and forwards from this account. ⌘B, ⌘I and ⇧⌘K format it. Saved as you type.") {
                SettingsBlock { SignatureEditor(core: model.core, accountID: section.account.id) }
            }
            if isIMAP {
                SettingsSection("Incoming mail (IMAP)") {
                    SettingsRow(title: "Server") { TextField("", text: binding(\.imapHost)).settingsField() }
                    SettingsRow(title: "Port") { TextField("", value: portBinding(\.imapPort), format: .number.grouping(.never)).settingsField(width: 80) }
                    SettingsRow(title: "Security") { securityPicker(\.imapSecurity, standard: (993, 143)) }
                    SettingsRow(title: "Username") { TextField("", text: binding(\.username)).settingsField() }
                    SettingsRow(title: "Password", detail: "Stored in your Keychain.") {
                        SecureField("", text: $password, prompt: Text("Unchanged")).settingsField()
                    }
                }
                SettingsSection("Outgoing mail (SMTP)") {
                    SettingsRow(title: "Server") { TextField("", text: binding(\.smtpHost)).settingsField() }
                    SettingsRow(title: "Port") { TextField("", value: portBinding(\.smtpPort), format: .number.grouping(.never)).settingsField(width: 80) }
                    SettingsRow(title: "Security") { securityPicker(\.smtpSecurity, standard: (465, 587)) }
                    SettingsRow(title: "Sign in with") {
                        Picker("Sign in with", selection: binding(\.smtpLogin)) {
                            Text("Same as incoming").tag(SmtpLogin.sameAsIncoming)
                            Text("Separate login").tag(SmtpLogin.separate)
                            Text("No login").tag(SmtpLogin.noAuth)
                        }
                        .settingsMenu()
                    }
                    if draft?.smtpLogin == .separate {
                        SettingsRow(title: "Username") { TextField("", text: binding(\.smtpUsername)).settingsField() }
                        SettingsRow(title: "Password") {
                            SecureField("", text: $smtpPassword, prompt: Text(saved?.smtpLogin == .separate ? "Unchanged" : "Required")).settingsField()
                        }
                    }
                }
                SettingsSection("Security") {
                    SettingsRow(title: "Verify server certificates",
                                detail: draft?.verifyCerts == false
                                    ? "Jolta accepts any certificate from these servers, so someone between you and them could read your mail and password. Only turn this off for a server you run yourself with a self-signed certificate."
                                    : "Jolta only connects to servers with a valid certificate.",
                                detailColor: draft?.verifyCerts == false ? .orange : Color(Theme.text3)) {
                        Toggle("Verify server certificates", isOn: binding(\.verifyCerts)).settingsSwitch()
                    }
                }
            } else {
                SettingsSection("Sign-in") {
                    let failure = model.syncErrors[section.id]
                    SettingsRow(title: "Signed in with \(section.account.provider)",
                                detail: failure.map { "This account can't sync: \($0). Sign in again to fix it." }
                                    ?? "Sign in again when Jolta reports that the account's access ran out or was revoked. \(section.account.provider) manages the servers and their security.",
                                detailColor: failure == nil ? Color(Theme.text3) : .orange) {
                        Button("Sign In Again…", action: signInAgain).disabled(signingIn)
                    }
                }
            }
            SettingsSection("Folders in the sidebar", footer: "Hidden folders still sync and show up in search.") {
                ForEach(folders, id: \.id) { box in
                    SettingsRow(title: box.name) {
                        Toggle(box.name, isOn: Binding(
                            get: { !box.hidden },
                            set: { show in try? model.core.setMailboxesHidden(mailboxIds: [box.id], hidden: !show) }
                        ))
                        .settingsSwitch()
                    }
                }
                if folders.isEmpty {
                    SettingsRow("No folders of its own", detail: "The inbox and the standard folders always show.")
                }
            } accessory: {
                HStack(spacing: 6) {
                    Button("Show All") { try? model.core.setMailboxesHidden(mailboxIds: folders.map(\.id), hidden: false) }
                    Button("Hide All") { try? model.core.setMailboxesHidden(mailboxIds: folders.map(\.id), hidden: true) }
                }
                .controlSize(.small)
                .disabled(folders.isEmpty)
            }
            SettingsSection("Remove") {
                SettingsRow(title: "Remove this account", detail: "Mail stays on the server. Jolta deletes its local copy and the account's Keychain entries.") {
                    Button("Remove…", role: .destructive) {
                        removal = Confirmation(
                            title: "Remove \(section.account.address)?",
                            message: "Mail stays on the server. Jolta deletes its local copy and the account's Keychain entries.",
                            action: "Remove Account") {
                                try? model.core.removeAccount(accountId: section.id)
                                model.reloadAccounts()
                                removed()
                            }
                    }
                }
            }
        }
        .safeAreaInset(edge: .bottom) {
            if changed || saving || message != nil {
                SaveBar(message: message?.text, failed: message?.failed ?? false, working: saving,
                        workingText: isIMAP ? "Checking the servers…" : "Saving…", revert: revert, save: save)
            }
        }
        .confirmation($removal)
    }

    private func binding<T>(_ path: WritableKeyPath<AccountSettingsInfo, T>) -> Binding<T> {
        Binding(get: { draft![keyPath: path] }, set: { draft![keyPath: path] = $0; message = nil })
    }

    private func portBinding(_ path: WritableKeyPath<AccountSettingsInfo, UInt16>) -> Binding<Int> {
        Binding(get: { Int(draft![keyPath: path]) }, set: { draft![keyPath: path] = UInt16(clamping: $0); message = nil })
    }

    /// Switching between TLS and STARTTLS also switches a standard port to the
    /// other standard port; a custom port stays.
    private func securityPicker(_ path: WritableKeyPath<AccountSettingsInfo, ConnectionSecurity>, standard: (tls: UInt16, starttls: UInt16)) -> some View {
        let port: WritableKeyPath<AccountSettingsInfo, UInt16> = path == \.imapSecurity ? \.imapPort : \.smtpPort
        return Picker("Security", selection: Binding(
            get: { draft![keyPath: path] },
            set: { new in
                let current = draft![keyPath: port]
                if new == .tls, current == standard.starttls { draft![keyPath: port] = standard.tls }
                if new == .startTls, current == standard.tls { draft![keyPath: port] = standard.starttls }
                draft![keyPath: path] = new
                message = nil
            }
        )) {
            Text("SSL/TLS").tag(ConnectionSecurity.tls)
            Text("STARTTLS").tag(ConnectionSecurity.startTls)
            Text("None").tag(ConnectionSecurity.plain)
        }
        .settingsMenu()
    }

    private func load() {
        do {
            let settings = try model.core.accountSettings(accountId: section.id)
            saved = settings
            draft = settings
        } catch {
            message = ("\(error)", true)
        }
    }

    private func revert() {
        draft = saved
        password = ""
        smtpPassword = ""
        message = nil
    }

    private func save() {
        guard let draft else { return }
        saving = true
        message = nil
        Task {
            do {
                _ = try await model.core.saveAccountSettings(
                    accountId: section.id, settings: draft,
                    password: password.isEmpty ? nil : password,
                    smtpPassword: smtpPassword.isEmpty ? nil : smtpPassword)
                saved = draft
                password = ""
                smtpPassword = ""
                message = ("Saved. Jolta reconnects with these settings.", false)
                model.reloadAccounts()
                Task {
                    try? await Task.sleep(for: .seconds(3))
                    if message?.failed == false { message = nil }
                }
            } catch {
                message = ("\(error)", true)
            }
            saving = false
        }
    }

    private func signInAgain() {
        signingIn = true
        message = ("Continue in your browser…", false)
        Task {
            do {
                let session = try await model.core.beginSignInAgain(accountId: section.id)
                if let url = URL(string: session.url()) { NSWorkspace.shared.open(url) }
                let account = try await session.finish()
                message = account.id == section.id
                    ? ("Signed in again. Jolta is syncing.", false)
                    : ("That signed in \(account.address), a different account; it was added separately.", true)
                model.reloadAccounts()
            } catch {
                message = ("Sign-in failed: \(error)", true)
            }
            signingIn = false
        }
    }
}

/// Settings › an account › Send mail as: the other addresses the account
/// sends from, and who new messages are from where accounts mix.
private struct SendAsSection: View {
    let model: AppModel
    let account: AccountInfo
    @State private var aliases: [AliasInfo] = []
    @State private var senders: [MailSender] = []
    @State private var defaultSender: SenderKey?
    @State private var address = ""
    @State private var name = ""
    @State private var error: String?
    @State private var fetchError: String?

    var body: some View {
        SettingsSection("Send mail as", footer: footer) {
            ForEach(aliases, id: \.id) { alias in
                SettingsBlock {
                    HStack(spacing: 10) {
                        VStack(alignment: .leading, spacing: 2) {
                            Text(alias.address)
                            Text(alias.fromProvider ? "From \(account.provider)" : "Added by you")
                                .font(.system(size: 11.5))
                                .foregroundStyle(Color(Theme.text3))
                        }
                        Spacer()
                        AliasNameField(model: model, alias: alias, placeholder: account.displayName)
                        Button("Remove") { remove(alias) }
                    }
                }
                .accessibilityIdentifier("alias-\(alias.address)")
            }
            SettingsBlock {
                HStack(spacing: 10) {
                    TextField("", text: $address, prompt: Text("name@example.com"))
                        .settingsField(width: nil)
                        .accessibilityIdentifier("alias-address")
                        .onSubmit(add)
                    TextField("", text: $name, prompt: Text("Name (optional)"))
                        .settingsField(width: 150)
                        .accessibilityIdentifier("alias-name")
                        .onSubmit(add)
                    Button("Add Address", action: add)
                        .disabled(!address.contains("@"))
                        .accessibilityIdentifier("alias-add")
                }
            }
            if let error {
                SettingsBlock { Text(error).foregroundStyle(.red).font(.system(size: 12)) }
            }
            if let fetchError {
                SettingsBlock {
                    Text("Jolta couldn't read \(account.provider)'s list: \(fetchError)").foregroundStyle(Color(Theme.text3)).font(.system(size: 12))
                }
            }
            if senders.count > 1 {
                SettingsRow(title: "New messages from",
                            detail: "In one account's folders, a new message is from that account. In the combined Inbox, Starred and other views that mix accounts, it is from this address.") {
                    Picker("New messages from", selection: Binding(get: { defaultSender }, set: setDefault)) {
                        SenderMenuItems(senders: senders)
                    }
                    .settingsMenu()
                    .accessibilityIdentifier("default-sender")
                }
            }
        }
        .task(id: account.id) {
            load()
            guard account.kind != "imap" else { return }
            do {
                aliases = try await model.core.refreshAliases(accountId: account.id)
                fetchError = nil
                loadSenders()
            } catch {
                fetchError = "\(error)"
            }
        }
    }

    private var footer: String {
        switch account.kind {
        case "gmail": "Jolta reads these from Gmail's Send mail as setting. A reply is from the address the message was sent to. Gmail sends mail from an address it doesn't list from \(account.address) instead."
        case "graph": "Jolta reads these from the addresses your organization gave this mailbox. A reply is from the address the message was sent to. Sending from one needs your administrator to allow it."
        default: "Add the addresses your provider lets this account send from, such as iCloud Mail aliases. A reply is from the address the message was sent to."
        }
    }

    private func load() {
        aliases = (try? model.core.aliases(accountId: account.id)) ?? []
        loadSenders()
    }

    private func loadSenders() {
        senders = (try? model.core.senders()) ?? []
        defaultSender = (try? model.core.defaultSender()).flatMap { $0 }.map { SenderKey(accountId: $0.accountId, alias: $0.alias) }
    }

    private func add() {
        do {
            _ = try model.core.addAlias(accountId: account.id, address: address, name: name)
            address = ""
            name = ""
            error = nil
        } catch {
            self.error = "\(error)"
        }
        load()
    }

    private func remove(_ alias: AliasInfo) {
        try? model.core.removeAlias(aliasId: alias.id)
        load()
    }

    private func setDefault(_ key: SenderKey?) {
        guard let key else { return }
        try? model.core.setDefaultSender(sender: SenderChoice(accountId: key.accountId, alias: key.alias))
        defaultSender = key
    }
}

/// An alias's name, saved a moment after each edit.
private struct AliasNameField: View {
    let model: AppModel
    let alias: AliasInfo
    let placeholder: String
    @State private var text = ""
    @State private var saveTask: Task<Void, Never>?

    var body: some View {
        TextField("", text: $text, prompt: Text(placeholder.isEmpty ? "Name" : placeholder))
            .settingsField(width: 150)
            .onAppear { text = alias.name }
            .onChange(of: text) {
                saveTask?.cancel()
                saveTask = Task {
                    try? await Task.sleep(for: .milliseconds(600))
                    guard !Task.isCancelled else { return }
                    try? model.core.renameAlias(aliasId: alias.id, name: text)
                }
            }
    }
}

/// Who a message is from, as a menu's tag: an account and maybe an alias.
struct SenderKey: Hashable {
    let accountId: Int64
    let alias: String?

    init(accountId: Int64, alias: String?) {
        self.accountId = accountId
        self.alias = alias?.lowercased()
    }

    init(_ sender: MailSender) {
        self.init(accountId: sender.accountId, alias: sender.alias)
    }
}

/// The menu items of a From menu: each account, its aliases under it.
struct SenderMenuItems: View {
    let senders: [MailSender]

    var body: some View {
        ForEach(Array(senders.enumerated()), id: \.offset) { index, sender in
            if sender.alias == nil && index > 0 { Divider() }
            Text(sender.alias == nil ? sender.address : "    \(sender.address)")
                .tag(Optional(SenderKey(sender)))
        }
    }
}
