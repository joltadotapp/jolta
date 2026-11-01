@preconcurrency import KiteFFI
import SwiftUI

/// Split-inbox sections: each is a name and a search query over the inbox.
struct SettingsView: View {
    let model: AppModel?

    var body: some View {
        TabView {
            AccountsSettings(model: model).tabItem { Label("Accounts", systemImage: "person.crop.circle") }
            SectionsSettings(model: model).tabItem { Label("Sections", systemImage: "rectangle.split.3x1") }
            VIPSettings(model: model).tabItem { Label("VIPs", systemImage: "checkmark.seal") }
            AgentsSettings(model: model).tabItem { Label("Agents", systemImage: "cpu") }
        }
        .frame(width: 860, height: 620)
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

/// The senders that matter most: a seal in the list, a VIP section, and
/// optionally the only mail that gets a banner.
struct VIPSettings: View {
    let model: AppModel?
    @State private var vips: [String] = []
    @State private var address = ""
    @State private var error: String?
    @AppStorage("NewMailBanners") private var banners = NewMailBanners.all

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("VIPs").font(.system(size: 13, weight: .semibold))
            Text("Mail from a VIP has a seal in front of the sender's name and shows in the VIP section. Press ! on a conversation to add or remove its sender.")
                .font(.system(size: 12))
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            List {
                ForEach(vips, id: \.self) { vip in
                    HStack {
                        Image(systemName: "checkmark.seal.fill").foregroundStyle(.secondary)
                        Text(vip)
                        Spacer()
                        Button("Remove") { set([vip], false) }
                    }
                }
                if vips.isEmpty {
                    Text("No VIPs yet").foregroundStyle(.secondary)
                }
            }
            .frame(minHeight: 220)
            HStack {
                TextField("", text: $address, prompt: Text("name@example.com"))
                    .frame(width: 280)
                    .onSubmit(add)
                Button("Add VIP", action: add).disabled(!address.contains("@"))
                if let error { Text(error).foregroundStyle(.red).font(.system(size: 12)).lineLimit(2) }
            }
            Divider().padding(.vertical, 6)
            Picker("New mail banners", selection: $banners) {
                Text("Every new message in the Inbox").tag(NewMailBanners.all)
                Text("Only mail from VIPs").tag(NewMailBanners.vips)
                Text("None").tag(NewMailBanners.none)
            }
            .pickerStyle(.radioGroup)
            Spacer()
        }
        .padding(20)
        .onAppear(perform: load)
    }

    private func load() {
        vips = (try? model?.core.vips()) ?? []
    }

    private func add() {
        let typed = address.trimmingCharacters(in: .whitespaces)
        guard typed.contains("@") else { return }
        set([typed], true)
        address = ""
    }

    private func set(_ addresses: [String], _ vip: Bool) {
        do {
            try model?.core.setVips(addresses: addresses, vip: vip)
            error = nil
        } catch {
            self.error = "\(error)"
        }
        load()
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
