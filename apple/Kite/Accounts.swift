@preconcurrency import KiteFFI
import SwiftUI

/// Settings › Accounts: the accounts on the left, the selected one's
/// connection, folders and removal on the right.
struct AccountsSettings: View {
    let model: AppModel?
    @State private var selection: Int64?
    @State private var addingIMAP = false
    @State private var signingIn = false
    @State private var status: String?

    private var accounts: [AccountSection] { model?.accounts ?? [] }

    var body: some View {
        HStack(spacing: 0) {
            VStack(spacing: 0) {
                List(selection: $selection) {
                    ForEach(accounts) { section in
                        VStack(alignment: .leading, spacing: 2) {
                            Text(section.title).fontWeight(.medium).lineLimit(1)
                            Text("\(section.account.provider) · \(section.account.address)")
                                .font(.system(size: 11))
                                .foregroundStyle(.secondary)
                                .lineLimit(1)
                                .truncationMode(.middle)
                        }
                        .padding(.vertical, 3)
                        .tag(section.id)
                    }
                }
                HStack(spacing: 6) {
                    Menu {
                        Button("Google Account…") { signIn(.google) }
                        Button("Microsoft 365 Account…") { signIn(.microsoft) }
                        Button("iCloud or IMAP Account…") { addingIMAP = true }
                    } label: {
                        Image(systemName: "plus")
                    }
                    .menuStyle(.borderlessButton)
                    .fixedSize()
                    .disabled(signingIn)
                    if signingIn { ProgressView().controlSize(.small) }
                    Spacer()
                }
                .padding(8)
                if let status {
                    Text(status).font(.system(size: 11)).foregroundStyle(.secondary).padding([.horizontal, .bottom], 8)
                }
            }
            .frame(width: 240)
            Divider()
            if let model, let section = accounts.first(where: { $0.id == selection }) {
                AccountDetail(model: model, section: section) { selection = nil }
                    .id(section.id)
            } else {
                Text(accounts.isEmpty ? "Add an account with +" : "Select an account")
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .onAppear { if selection == nil { selection = accounts.first?.id } }
        .sheet(isPresented: $addingIMAP) {
            AddIMAPAccount(model: model) { added in
                addingIMAP = false
                if let added {
                    status = "Added \(added.address). Kite is syncing it now."
                    selection = added.id
                }
            }
        }
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
                selection = account.id
            } catch {
                status = "Sign-in failed: \(error)"
            }
            signingIn = false
        }
    }
}

/// One account: its name, its servers (IMAP only), its folders.
private struct AccountDetail: View {
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
            form
        } else {
            ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity).onAppear(perform: load)
        }
    }

    private var form: some View {
        Form {
            Section {
                LabeledContent("Provider", value: section.account.provider)
                TextField("Name", text: binding(\.displayName), prompt: Text("Shown as the sender"))
                if isIMAP {
                    TextField("Email address", text: binding(\.address))
                } else {
                    LabeledContent("Email address", value: section.account.address)
                }
            }
            if isIMAP {
                Section("Incoming mail (IMAP)") {
                    TextField("Server", text: binding(\.imapHost))
                    TextField("Port", value: portBinding(\.imapPort), format: .number.grouping(.never))
                    securityPicker(\.imapSecurity, standard: (993, 143))
                    TextField("Username", text: binding(\.username))
                    SecureField("Password", text: $password, prompt: Text("Unchanged"))
                }
                Section("Outgoing mail (SMTP)") {
                    TextField("Server", text: binding(\.smtpHost))
                    TextField("Port", value: portBinding(\.smtpPort), format: .number.grouping(.never))
                    securityPicker(\.smtpSecurity, standard: (465, 587))
                    Picker("Sign in with", selection: binding(\.smtpLogin)) {
                        Text("Same as incoming").tag(SmtpLogin.sameAsIncoming)
                        Text("Separate login").tag(SmtpLogin.separate)
                        Text("No login").tag(SmtpLogin.noAuth)
                    }
                    if draft?.smtpLogin == .separate {
                        TextField("Username", text: binding(\.smtpUsername))
                        SecureField("Password", text: $smtpPassword, prompt: Text(saved?.smtpLogin == .separate ? "Unchanged" : "Required"))
                    }
                }
                Section {
                    Toggle("Verify server certificates", isOn: binding(\.verifyCerts))
                } footer: {
                    if draft?.verifyCerts == false {
                        Text("Kite accepts any certificate from these servers, so someone between you and them could read your mail and password. Only turn this off for a server you run yourself with a self-signed certificate.")
                            .foregroundStyle(.orange)
                    }
                }
            } else {
                Section("Sign-in") {
                    LabeledContent("Signed in with \(section.account.provider)") {
                        Button("Sign In Again…", action: signInAgain).disabled(signingIn)
                    }
                    Text("Sign in again when Kite reports that the account's access ran out or was revoked. Servers and security are managed by \(section.account.provider).")
                        .font(.system(size: 11))
                        .foregroundStyle(.secondary)
                }
            }
            Section {
                HStack {
                    if saving { ProgressView().controlSize(.small); Text(isIMAP ? "Checking the servers…" : "Saving…").foregroundStyle(.secondary) }
                    if let message { Text(message.text).foregroundStyle(message.failed ? .red : .secondary).lineLimit(3) }
                    Spacer()
                    Button("Revert", action: revert).disabled(!changed || saving)
                    Button("Save", action: save).keyboardShortcut(.defaultAction).disabled(!changed || saving)
                }
            }
            Section {
                if folders.isEmpty {
                    Text("No folders of its own").foregroundStyle(.secondary)
                }
                ForEach(folders, id: \.id) { box in
                    Toggle(box.name, isOn: Binding(
                        get: { !box.hidden },
                        set: { show in try? model.core.setMailboxesHidden(mailboxIds: [box.id], hidden: !show) }
                    ))
                    .toggleStyle(.checkbox)
                }
            } header: {
                HStack {
                    Text("Folders in the sidebar")
                    Spacer()
                    Button("Show All") { try? model.core.setMailboxesHidden(mailboxIds: folders.map(\.id), hidden: false) }
                    Button("Hide All") { try? model.core.setMailboxesHidden(mailboxIds: folders.map(\.id), hidden: true) }
                }
                .disabled(folders.isEmpty)
            } footer: {
                Text("Hidden folders still sync and show up in search.").foregroundStyle(.secondary)
            }
            Section {
                Button("Remove Account…", role: .destructive) { confirmRemove = true }
            }
        }
        .formStyle(.grouped)
        .confirmationDialog("Remove \(section.account.address)?", isPresented: $confirmRemove) {
            Button("Remove account and its local mail", role: .destructive) {
                try? model.core.removeAccount(accountId: section.id)
                removed()
            }
        } message: {
            Text("Mail stays on the server. Kite deletes its local copy and the account's Keychain entries.")
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
                message = ("Saved. Kite reconnects with these settings.", false)
                model.reloadAccounts()
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
                    ? ("Signed in again. Kite is syncing.", false)
                    : ("That signed in \(account.address), a different account; it was added separately.", true)
                model.reloadAccounts()
            } catch {
                message = ("Sign-in failed: \(error)", true)
            }
            signingIn = false
        }
    }
}
