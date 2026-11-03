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
    @State private var confirmRemove = false
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
                    SettingsRow(title: "Signed in with \(section.account.provider)",
                                detail: "Sign in again when Jolta reports that the account's access ran out or was revoked. \(section.account.provider) manages the servers and their security.") {
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
                    Button("Remove…", role: .destructive) { confirmRemove = true }
                }
            }
        }
        .safeAreaInset(edge: .bottom) {
            if changed || saving || message != nil {
                SaveBar(message: message?.text, failed: message?.failed ?? false, working: saving,
                        workingText: isIMAP ? "Checking the servers…" : "Saving…", revert: revert, save: save)
            }
        }
        .confirmationDialog("Remove \(section.account.address)?", isPresented: $confirmRemove) {
            Button("Remove account and its local mail", role: .destructive) {
                try? model.core.removeAccount(accountId: section.id)
                model.reloadAccounts()
                removed()
            }
        } message: {
            Text("Mail stays on the server. Jolta deletes its local copy and the account's Keychain entries.")
        }
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
        let provider: OAuthProvider = section.account.kind == "gmail" ? .google : .microsoft
        signingIn = true
        message = ("Continue in your browser…", false)
        Task {
            do {
                let session = try await model.core.beginSignIn(provider: provider)
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
