@preconcurrency import JoltaFFI
import SwiftUI

/// Split-inbox sections: each is a name and a search query over the inbox.
struct SectionsSettings: View {
    let model: AppModel
    @State private var sections: [SectionInfo] = []
    @State private var saved: [SectionInfo] = []
    @State private var error: String?

    var body: some View {
        SettingsScroll {
            SettingsSection("Inbox sections",
                            footer: "Each section is a tab over the inbox showing the conversations its query matches. Queries use the search language: from:, to:, subject:, has:attachment, has:list, is:unread, is:vip, is:newsletter, is:notification.") {
                ForEach($sections) { $s in
                    SettingsBlock {
                        HStack(spacing: 10) {
                            TextField("Name", text: $s.name).settingsField(width: 150)
                            TextField("Query", text: $s.query).settingsField(width: nil, mono: true)
                            Button { sections.removeAll { $0.id == s.id } } label: { Image(systemName: "trash") }
                                .buttonStyle(.plain)
                                .foregroundStyle(Color(Theme.text3))
                                .help("Remove this section")
                        }
                    }
                }
                if sections.isEmpty {
                    SettingsRow("No sections", detail: "The inbox shows one list. Add a section to split it into tabs.")
                }
            } accessory: {
                Button("Add Section") {
                    let unused = min(0, sections.map(\.id).min() ?? 0) - 1
                    sections.append(SectionInfo(id: unused, name: "New section", query: "from:"))
                }
            }
        }
        .safeAreaInset(edge: .bottom) {
            if sections != saved || error != nil {
                SaveBar(message: error, failed: true, revert: { sections = saved; error = nil }) {
                    do {
                        saved = try model.saveSections(sections)
                        sections = saved
                        error = nil
                    } catch {
                        self.error = "\(error)"
                    }
                }
            }
        }
        .onAppear {
            saved = model.sections
            sections = saved
        }
    }
}

extension SectionInfo: @retroactive Identifiable {}

/// Save and Revert under a page whose changes apply together.
struct SaveBar: View {
    var message: String?
    var failed = false
    var working = false
    var workingText = "Saving…"
    let revert: () -> Void
    let save: () -> Void

    var body: some View {
        HStack(spacing: 10) {
            if working {
                ProgressView().controlSize(.small)
                Text(workingText).foregroundStyle(Color(Theme.text3))
            } else if let message {
                Text(message).foregroundStyle(failed ? Color.red : Color(Theme.text3)).lineLimit(2)
            } else {
                Text("Unsaved changes").foregroundStyle(Color(Theme.text3))
            }
            Spacer()
            Button("Revert", action: revert).disabled(working)
            Button("Save", action: save).keyboardShortcut(.defaultAction).tint(Color(Theme.accent)).disabled(working)
        }
        .font(.system(size: 12.5))
        .padding(.horizontal, 32)
        .frame(height: 52)
        .background(Color(Theme.bg))
        .overlay(alignment: .top) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
    }
}

/// The senders that matter most: a seal in the list and a VIP section.
struct VIPSettings: View {
    let model: AppModel
    @State private var vips: [String] = []
    @State private var address = ""
    @State private var error: String?

    var body: some View {
        SettingsScroll {
            SettingsSection("VIPs", footer: "Mail from a VIP has a seal in front of the sender's name and shows in the VIP section. Press ! on a conversation to add or remove its sender. General › New mail banners can limit banners to VIPs.") {
                ForEach(vips, id: \.self) { vip in
                    SettingsBlock {
                        HStack(spacing: 10) {
                            Image(systemName: "checkmark.seal.fill").foregroundStyle(Color(Theme.text3))
                            Text(vip)
                            Spacer()
                            Button("Remove") { set([vip], false) }
                        }
                    }
                }
                SettingsBlock {
                    HStack(spacing: 10) {
                        TextField("", text: $address, prompt: Text("name@example.com")).settingsField(width: nil).onSubmit(add)
                        Button("Add VIP", action: add).disabled(!address.contains("@"))
                    }
                }
                if let error {
                    SettingsBlock { Text(error).foregroundStyle(.red).font(.system(size: 12)) }
                }
            }
        }
        .onAppear(perform: load)
    }

    private func load() {
        vips = (try? model.core.vips()) ?? []
    }

    private func add() {
        let typed = address.trimmingCharacters(in: .whitespaces)
        guard typed.contains("@") else { return }
        set([typed], true)
        address = ""
    }

    private func set(_ addresses: [String], _ vip: Bool) {
        do {
            try model.core.setVips(addresses: addresses, vip: vip)
            error = nil
        } catch {
            self.error = "\(error)"
        }
        load()
    }
}

/// Reusable text for the compose window (⌘;), saved as you type.
struct SnippetsSettings: View {
    let model: AppModel
    @State private var snippets: [SnippetInfo] = []
    @State private var selected: Int64?
    @State private var name = ""
    @State private var text = ""
    @State private var saveTask: Task<Void, Never>?

    var body: some View {
        SettingsScroll {
            SettingsSection("Snippets", footer: "Press ⌘; while writing to insert one at the cursor.") {
                ForEach(snippets, id: \.id) { s in
                    Button { pick(s.id) } label: {
                        SettingsBlock {
                            HStack(spacing: 12) {
                                Text(s.name.isEmpty ? "Untitled" : s.name).fontWeight(.medium)
                                Text(preview(s.body)).foregroundStyle(Color(Theme.text3)).lineLimit(1)
                                Spacer()
                                if s.id == selected { Image(systemName: "pencil").foregroundStyle(Color(Theme.accent)) }
                            }
                        }
                        .background(s.id == selected ? Color(Theme.fill) : .clear)
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                }
                if snippets.isEmpty {
                    SettingsRow("No snippets yet", detail: "Snippets are text you reuse while writing, like a thank-you or directions.")
                }
            } accessory: {
                Button("New Snippet", action: add)
            }
            if selected != nil {
                SettingsSection("Edit snippet", footer: "{first name}, {name} and {email} fill in from the message's first recipient. ⌘B, ⌘I and ⇧⌘K format the text. Saved as you type.") {
                    SettingsRow(title: "Name", detail: "What you type to find it.") {
                        TextField("", text: $name).settingsField().onChange(of: name) { scheduleSave() }
                    }
                    SettingsBlock {
                        RichTextEditor(text: $text, core: model.core, fontSize: 13)
                            .settingsEditor(height: 180)
                            .onChange(of: text) { scheduleSave() }
                    }
                } accessory: {
                    Button("Delete", role: .destructive, action: remove)
                }
            }
        }
        .onAppear(perform: load)
        .onDisappear { if let selected { save(selected) } }
    }

    private func preview(_ body: String) -> String {
        model.core.bodySpans(body: body).map(\.text).joined().split(separator: "\n").first.map(String.init) ?? ""
    }

    private func load() {
        snippets = (try? model.core.snippets()) ?? []
        if selected == nil { pick(snippets.first?.id) }
    }

    private func pick(_ id: Int64?) {
        if let old = selected, old != id { save(old) }
        saveTask?.cancel()
        selected = id
        let s = snippets.first { $0.id == id }
        name = s?.name ?? ""
        text = s?.body ?? ""
    }

    private func add() {
        guard let id = try? model.core.saveSnippet(snippet: SnippetInfo(id: 0, name: "New snippet", body: "")) else { return }
        if let old = selected { save(old) }
        snippets = (try? model.core.snippets()) ?? []
        pick(id)
    }

    private func remove() {
        guard let id = selected else { return }
        saveTask?.cancel()
        try? model.core.deleteSnippet(id: id)
        snippets = (try? model.core.snippets()) ?? []
        selected = nil
        pick(snippets.first?.id)
    }

    private func scheduleSave() {
        guard let id = selected else { return }
        saveTask?.cancel()
        saveTask = Task {
            try? await Task.sleep(for: .milliseconds(600))
            guard !Task.isCancelled else { return }
            save(id)
        }
    }

    private func save(_ id: Int64) {
        guard let index = snippets.firstIndex(where: { $0.id == id }),
              snippets[index].name != name || snippets[index].body != text else { return }
        let updated = SnippetInfo(id: id, name: name, body: text)
        _ = try? model.core.saveSnippet(snippet: updated)
        snippets[index] = updated
    }
}

/// Email and password; the servers too when Jolta doesn't know them from the
/// address. Jolta signs in before it saves anything.
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
                 : "Jolta checks the password with the server before saving it in your Keychain.")
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
