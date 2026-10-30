@preconcurrency import KiteFFI
import SwiftUI

/// Split-inbox sections: each is a name and a search query over the inbox.
struct SettingsView: View {
    let model: AppModel?

    var body: some View {
        TabView {
            AccountsSettings(model: model).tabItem { Label("Accounts", systemImage: "person.crop.circle") }
            SectionsSettings(model: model).tabItem { Label("Sections", systemImage: "rectangle.split.3x1") }
            AgentsSettings(model: model).tabItem { Label("Agents", systemImage: "cpu") }
        }
        .frame(width: 620, height: 460)
    }
}

struct SectionsSettings: View {
    let model: AppModel?
    @State private var sections: [SectionInfo] = []
    @State private var error: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Inbox sections").font(.system(size: 13, weight: .semibold))
            Text("Each section is a tab over the inbox showing the conversations its query matches. Queries use the search language: from:, to:, subject:, has:attachment, has:list, is:unread, is:vip, is:newsletter, is:notification.")
                .font(.system(size: 12))
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            Table($sections) {
                TableColumn("Name") { $s in TextField("", text: $s.name) }.width(140)
                TableColumn("Query") { $s in TextField("", text: $s.query).font(.system(size: 12, design: .monospaced)) }
            }
            .frame(minHeight: 180)
            HStack {
                Button("Add") { sections.append(SectionInfo(id: -Int64(sections.count + 1), name: "New section", query: "from:")) }
                Button("Remove last") { if !sections.isEmpty { sections.removeLast() } }
                Spacer()
                if let error { Text(error).foregroundStyle(.red).font(.system(size: 12)).lineLimit(2) }
                Button("Save") {
                    do {
                        try model?.saveSections(sections)
                        error = nil
                    } catch {
                        self.error = "\(error)"
                    }
                }
                .keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
        .onAppear { sections = model?.sections ?? [] }
    }
}

extension SectionInfo: @retroactive Identifiable {}

/// Accounts: add one through the browser, or remove one with its mail.
struct AccountsSettings: View {
    let model: AppModel?
    @State private var status: String?
    @State private var signingIn = false
    @State private var confirmRemove: AccountInfo?
    @State private var addingIMAP = false

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            ForEach(model?.accounts ?? []) { section in
                HStack {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(section.title).fontWeight(.medium)
                        Text("\(section.account.provider) · \(section.account.address)").font(.system(size: 11)).foregroundStyle(.secondary)
                    }
                    Spacer()
                    Button("Remove…") { confirmRemove = section.account }
                }
            }
            Divider()
            HStack {
                Button("Add Google account…") { signIn(.google) }.disabled(signingIn)
                Button("Add Microsoft 365 account…") { signIn(.microsoft) }.disabled(signingIn)
                Button("Add iCloud or IMAP account…") { addingIMAP = true }.disabled(signingIn)
                if signingIn { ProgressView().controlSize(.small) }
            }
            if let status { Text(status).font(.system(size: 12)).foregroundStyle(.secondary) }
            Spacer()
        }
        .padding(20)
        .sheet(isPresented: $addingIMAP) {
            AddIMAPAccount(model: model) { added in
                addingIMAP = false
                if let added { status = "Added \(added.address). Kite is syncing it now." }
            }
        }
        .confirmationDialog("Remove \(confirmRemove?.address ?? "")?", isPresented: Binding(get: { confirmRemove != nil }, set: { if !$0 { confirmRemove = nil } })) {
            Button("Remove account and its local mail", role: .destructive) {
                if let a = confirmRemove { try? model?.core.removeAccount(accountId: a.id) }
                confirmRemove = nil
            }
        } message: {
            Text("Mail stays on the server. Kite deletes its local copy and the account's Keychain entry.")
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
                status = "Added \(account.address). Kite is syncing it now."
                model?.reloadAccounts()
            } catch {
                status = "Sign-in failed: \(error)"
            }
            signingIn = false
        }
    }
}

/// Email and password; the servers too when Kite doesn't know them from the
/// address. Kite signs in before it saves anything.
struct AddIMAPAccount: View {
    let model: AppModel?
    let done: (AccountInfo?) -> Void
    @State private var address = ""
    @State private var password = ""
    @State private var imapHost = ""
    @State private var smtpHost = ""
    @State private var working = false
    @State private var failure: String?

    private var knowsServers: Bool { model?.core.knowsServers(address: address) ?? false }
    private var needsServers: Bool { address.contains("@") && !knowsServers }
    private var ready: Bool {
        address.contains("@") && !password.isEmpty && (!needsServers || !imapHost.isEmpty) && !working
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Add an iCloud or IMAP account").font(.headline)
            Form {
                TextField("Email", text: $address, prompt: Text("you@icloud.com"))
                    .textContentType(.emailAddress)
                SecureField("Password", text: $password, prompt: Text("App-specific password"))
                if needsServers {
                    TextField("IMAP server", text: $imapHost, prompt: Text("imap.example.com"))
                    TextField("SMTP server", text: $smtpHost, prompt: Text("Same as IMAP with smtp"))
                }
            }
            .disabled(working)
            Text(address.contains("@") && knowsServers
                 ? "iCloud and Fastmail need an app-specific password, not your account password. Create one at account.apple.com or in Fastmail's Settings › Privacy & Security."
                 : "Kite checks the password with the server before saving it in your Keychain.")
                .font(.system(size: 11))
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            if let failure {
                Text(failure).font(.system(size: 12)).foregroundStyle(.red).fixedSize(horizontal: false, vertical: true)
            }
            HStack {
                if working { ProgressView().controlSize(.small); Text("Signing in…").foregroundStyle(.secondary) }
                Spacer()
                Button("Cancel") { done(nil) }.keyboardShortcut(.cancelAction)
                Button("Add Account") { add() }.keyboardShortcut(.defaultAction).disabled(!ready)
            }
        }
        .padding(20)
        .frame(width: 420)
    }

    private func add() {
        guard let core = model?.core else { return }
        working = true
        failure = nil
        Task {
            do {
                let added = try await core.addImapAccount(address: address, password: password,
                                                          imapHost: needsServers ? imapHost : "",
                                                          smtpHost: needsServers ? smtpHost : "")
                model?.reloadAccounts()
                done(added)
            } catch {
                failure = "\(error)"
            }
            working = false
        }
    }
}
