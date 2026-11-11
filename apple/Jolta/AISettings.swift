@preconcurrency import JoltaFFI
import SwiftUI

/// Settings › AI: which model writes summaries and drafts. A model on this
/// Mac (downloaded here), or the user's own OpenAI or Anthropic account.
struct AISettings: View {
    let model: AppModel

    var body: some View {
        SettingsScroll {
            SettingsSection("Model", footer: footer) {
                SettingsBlock { ProviderPicker(model: model) }
            }
            switch model.ai.provider {
            case .local: LocalModels(model: model)
            case .openAi: RemoteAccount(model: model, provider: .openAi).id(AiProvider.openAi)
            case .anthropic: RemoteAccount(model: model, provider: .anthropic).id(AiProvider.anthropic)
            case nil: EmptyView()
            }
            SettingsSection("Features") {
                SortMailRow(model: model)
                SettingsRow(title: "Summarize long conversations as they open",
                            detail: "Three or more messages, or one long one. Summaries are kept and catch up when replies arrive. ⌘J summarizes any conversation.") {
                    Toggle("Summarize long conversations", isOn: Binding(get: { model.ai.autoSummary },
                                                                         set: { on in model.saveAI { $0.autoSummary = on } }))
                        .settingsSwitch()
                        .disabled(model.ai.provider == nil)
                }
                SettingsRow(title: "Write with AI",
                            detail: "Press ⌘J while writing and say what the message should say, or jot rough notes in the message first.") {
                    KeyCap(key: "⌘J")
                }
            }
            .opacity(model.ai.provider == nil ? 0.5 : 1)
        }
    }

    private var footer: String {
        switch model.ai.provider {
        case nil: "AI is off. Jolta never reads your mail with a model until you choose one."
        case .local: "Runs on this Mac. Nothing you read or write leaves it."
        case .openAi: "Jolta sends the conversation you summarize or reply to from this Mac straight to OpenAI, under your account and its API terms. It never passes through a server of Jolta's."
        case .anthropic: "Jolta sends the conversation you summarize or reply to from this Mac straight to Anthropic, under your account and its API terms. It never passes through a server of Jolta's."
        }
    }
}

/// Off, this Mac, OpenAI or Anthropic, as four tiles.
private struct ProviderPicker: View {
    let model: AppModel

    private struct Choice: Identifiable {
        let provider: AiProvider?
        let title: String
        let detail: String
        let icon: String
        var id: String { title }
    }

    private let choices = [
        Choice(provider: nil, title: "Off", detail: "No summaries or drafts", icon: "circle.slash"),
        Choice(provider: .local, title: "On this Mac", detail: "Private and free", icon: "laptopcomputer"),
        Choice(provider: .openAi, title: "OpenAI", detail: "Your API key", icon: "cloud"),
        Choice(provider: .anthropic, title: "Anthropic", detail: "Your API key", icon: "cloud"),
    ]

    var body: some View {
        HStack(spacing: 8) {
            ForEach(choices) { choice in
                ChoiceCard(title: choice.title, detail: choice.detail, icon: choice.icon, selected: model.ai.provider == choice.provider) {
                    model.saveAI { $0.provider = choice.provider; $0.sortMail = nil }
                }
            }
        }
    }
}

/// Sorting new mail: needs reply and categories, with the Needs reply
/// section one click away.
private struct SortMailRow: View {
    let model: AppModel
    @State private var pending: UInt32 = 0

    private var hasSection: Bool { model.sections.contains { $0.query.contains("is:needs-reply") } }

    private var detail: String {
        let what = "Marks conversations that wait for your reply (↩ in the list) and files new mail by category, for search and sections: is:needs-reply, category:work, category:finance."
        let catchUp = "Turning it on also sorts the last two weeks of your inbox."
        switch model.ai.provider {
        case .local: return "\(what) Runs on this Mac, about half a second per message. \(catchUp)"
        case .openAi: return "\(what) Each new message goes to OpenAI once to be sorted. \(catchUp)"
        case .anthropic: return "\(what) Each new message goes to Anthropic once to be sorted. \(catchUp)"
        case nil: return what
        }
    }

    var body: some View {
        SettingsRow(title: "Sort new mail", detail: detail) {
            Toggle("Sort new mail", isOn: Binding(get: { model.ai.sortsMail }, set: { on in model.saveAI { $0.sortMail = on } }))
                .settingsSwitch()
                .disabled(model.ai.provider == nil)
        }
        if model.ai.sortsMail {
            SettingsRow(title: pending > 0 ? "Sorting \(pending) message\(pending == 1 ? "" : "s")…" : "Needs reply section",
                        detail: hasSection ? "Your inbox has a Needs reply section." : "A section above the inbox with only the conversations that wait for you.") {
                if !hasSection {
                    Button("Add Section") {
                        let list = model.sections + [SectionInfo(id: 0, name: "Needs reply", query: "is:needs-reply")]
                        try? model.saveSections(list)
                    }
                }
            }
            .task {
                while !Task.isCancelled {
                    pending = model.core.aiBackgroundPending()
                    try? await Task.sleep(for: .seconds(1))
                }
            }
        }
    }
}

// MARK: - On this Mac

private struct LocalModels: View {
    let model: AppModel
    @State private var lowSpace: String?

    private var appleSilicon: Bool {
        var info = utsname()
        uname(&info)
        return withUnsafeBytes(of: &info.machine) { String(decoding: $0.prefix { $0 != 0 }, as: UTF8.self) } == "arm64"
    }

    var body: some View {
        SettingsSection("Model on this Mac",
                        footer: "Models come from Hugging Face and are checked before first use. A model stays loaded for 5 minutes after it was last used, then gives its memory back.") {
            if !appleSilicon {
                SettingsRow("Needs a Mac with Apple silicon",
                            detail: "This Mac can't run local models fast enough. Use your OpenAI or Anthropic account instead.")
            } else {
                ForEach(model.localModels, id: \.id) { row($0) }
            }
        }
        .alert("Not enough free space", isPresented: Binding(get: { lowSpace != nil }, set: { if !$0 { lowSpace = nil } })) {
            Button("OK") {}
        } message: {
            Text(lowSpace ?? "")
        }
    }

    private func row(_ m: LocalModelInfo) -> some View {
        let chosen = model.ai.localModel == m.id
        return HStack(alignment: .center, spacing: 12) {
            Button { model.saveAI { $0.localModel = m.id } } label: {
                Image(systemName: chosen ? "largecircle.fill.circle" : "circle")
                    .font(.system(size: 14))
                    .foregroundStyle(Color(chosen ? Theme.text : Theme.text3))
            }
            .buttonStyle(.plain)
            .help(chosen ? "Jolta uses this model" : "Use this model")
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    Text(m.name).font(.system(size: 13, weight: .medium))
                    if m.id == model.localModels.first?.id {
                        Text("Recommended")
                            .font(.system(size: 10.5, weight: .medium))
                            .padding(.horizontal, 6)
                            .frame(height: 17)
                            .background(Capsule().fill(Color(Theme.fill)))
                    }
                }
                Text(m.detail).font(.system(size: 12)).foregroundStyle(Color(Theme.text2))
                Text(stateLine(m)).font(.system(size: 11.5).monospacedDigit()).foregroundStyle(stateColor(m))
                    .fixedSize(horizontal: false, vertical: true)
                if case .downloading(let done, _) = m.state {
                    ProgressView(value: Double(done), total: Double(m.size)).progressViewStyle(.linear).tint(Color(Theme.text2))
                        .frame(maxWidth: 320)
                }
            }
            Spacer(minLength: 12)
            controls(m)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 12)
        .contentShape(Rectangle())
        .onTapGesture { model.saveAI { $0.localModel = m.id } }
    }

    private func stateLine(_ m: LocalModelInfo) -> String {
        let size = Self.bytes(m.size)
        switch m.state {
        case .notDownloaded: return "\(size) download · uses about \(m.memory) of memory while it runs"
        case .paused(let done): return "Paused at \(Self.bytes(done)) of \(size)"
        case .downloading(let done, let speed):
            guard speed > 0 else { return "Downloading \(Self.bytes(done)) of \(size)…" }
            let left = Double(m.size - min(done, m.size)) / speed
            return "\(Self.bytes(done)) of \(size) · \(Self.bytes(UInt64(speed)))/s · \(Self.timeLeft(left))"
        case .verifying: return "Checking the file…"
        case .ready: return "Downloaded · \(size) on disk"
        case .failed(let message): return message
        }
    }

    private func stateColor(_ m: LocalModelInfo) -> Color {
        if case .failed = m.state { return Color(Theme.dangerText) }
        return Color(Theme.text3)
    }

    @ViewBuilder private func controls(_ m: LocalModelInfo) -> some View {
        switch m.state {
        case .notDownloaded, .failed:
            Button("Download") { download(m) }
        case .paused:
            HStack(spacing: 6) {
                Button("Resume") { download(m) }
                Button("Cancel") { try? model.core.removeModel(id: m.id) }
            }
        case .downloading:
            Button("Pause") { model.core.pauseDownload(id: m.id) }
        case .verifying:
            ProgressView().controlSize(.small)
        case .ready:
            Menu {
                Button("Remove from This Mac…") { remove(m) }
            } label: {
                Image(systemName: "ellipsis.circle").font(.system(size: 14))
            }
            .menuStyle(.button)
            .buttonStyle(.plain)
            .menuIndicator(.hidden)
            .foregroundStyle(Color(Theme.text2))
            .help("More")
            .fixedSize()
        }
    }

    /// Starts or resumes a download once there is room for it, and picks the model.
    private func download(_ m: LocalModelInfo) {
        let folder = URL(fileURLWithPath: AppModel.dataDirectory)
        let done: UInt64 = if case .paused(let done) = m.state { done } else { 0 }
        let needed = Int64(m.size - done) + 500_000_000
        if let free = try? folder.resourceValues(forKeys: [.volumeAvailableCapacityForImportantUsageKey]).volumeAvailableCapacityForImportantUsage,
           free < needed {
            lowSpace = "\(m.name) needs \(Self.bytes(UInt64(needed))) and this Mac has \(Self.bytes(UInt64(max(free, 0)))) free."
            return
        }
        model.saveAI { $0.localModel = m.id }
        model.core.downloadModel(id: m.id)
    }

    private func remove(_ m: LocalModelInfo) {
        let alert = NSAlert()
        alert.messageText = "Remove \(m.name)?"
        alert.informativeText = "This frees \(Self.bytes(m.size)). Summaries already made stay. You can download it again any time."
        alert.addButton(withTitle: "Remove")
        alert.addButton(withTitle: "Cancel")
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        try? model.core.removeModel(id: m.id)
    }

    static func bytes(_ n: UInt64) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(n), countStyle: .file)
    }

    static func timeLeft(_ seconds: Double) -> String {
        switch seconds {
        case ..<60: "less than a minute left"
        case ..<3600: "about \(Int((seconds / 60).rounded())) min left"
        default: String(format: "about %.1f h left", seconds / 3600)
        }
    }
}

// MARK: - OpenAI and Anthropic

private struct RemoteAccount: View {
    let model: AppModel
    let provider: AiProvider
    @State private var key = ""
    @State private var replacing = false
    @State private var checking = false
    @State private var problem: String?
    @State private var models: [AiModelInfo] = []
    @State private var address = ""
    @State private var addressProblem: String?
    @State private var advanced = false

    private var name: String { provider == .openAi ? "OpenAI" : "Anthropic" }
    private var saved: Bool { provider == .openAi ? model.ai.openaiKeySaved : model.ai.anthropicKeySaved }
    private var url: String { provider == .openAi ? model.ai.openaiUrl : model.ai.anthropicUrl }
    private var chosen: String { provider == .openAi ? model.ai.openaiModel : model.ai.anthropicModel }
    private var keysPage: URL {
        URL(string: provider == .openAi ? "https://platform.openai.com/api-keys" : "https://console.anthropic.com/settings/keys")!
    }

    var body: some View {
        SettingsSection("\(name) account") {
            SettingsRow(title: "API key", detail: keyDetail, detailColor: problem == nil ? Color(Theme.text3) : Color(Theme.dangerText)) {
                keyControl
            }
            SettingsRow(title: "Model", detail: modelDetail) { modelControl }
        } accessory: {
            KitButton(title: "Get a key ↗", kind: .quiet, size: .small) { NSWorkspace.shared.open(keysPage) }
                .padding(.trailing, -7)
        }
        VStack(alignment: .leading, spacing: 10) {
            Button { withAnimation(.easeInOut(duration: 0.15)) { advanced.toggle() } } label: {
                HStack(spacing: 6) {
                    Image(systemName: "chevron.right").font(.system(size: 10, weight: .semibold)).rotationEffect(.degrees(advanced ? 90 : 0))
                    Text("Advanced").font(.system(size: 13, weight: .semibold))
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .padding(.horizontal, 4)
            if advanced {
                SettingsCard {
                    SettingsRow(title: "API address",
                                detail: addressProblem ?? "For a compatible server, such as Ollama, LM Studio or a company proxy. A server on your own network may not need a key.",
                                detailColor: addressProblem == nil ? Color(Theme.text3) : Color(Theme.dangerText)) {
                        HStack(spacing: 6) {
                            TextField(model.core.aiDefaultUrl(provider: provider), text: $address)
                                .settingsField(width: 220, mono: true)
                                .onSubmit(saveAddress)
                            if address != url { Button("Save", action: saveAddress) }
                            else if !url.isEmpty { Button("Reset") { address = ""; saveAddress() } }
                        }
                        .fixedSize()
                    }
                }
            }
        }
        .task {
            address = url
            advanced = !url.isEmpty
            if saved || !url.isEmpty { await loadModels(key: nil) }
        }
    }

    private var keyDetail: String {
        if let problem { return problem }
        if checking { return "Checking the key with \(name)…" }
        if saved && !replacing { return "Saved in your Mac's Keychain. Jolta only shows that it's there." }
        if !url.isEmpty { return "Optional for a server on your own network. Jolta keeps a key in your Mac's Keychain." }
        return "Paste a key from your \(name) account. Jolta keeps it in your Mac's Keychain."
    }

    @ViewBuilder private var keyControl: some View {
        if saved && !replacing {
            HStack(spacing: 6) {
                Text("••••••••••••").font(.system(size: 12.5, design: .monospaced)).foregroundStyle(Color(Theme.text2))
                Button("Replace…") { replacing = true; key = "" }
                Button("Remove") {
                    try? model.core.setAiKey(provider: provider, key: "")
                    models = []
                    model.reloadAI()
                }
            }
        } else {
            HStack(spacing: 6) {
                SecureField(provider == .openAi ? "sk-…" : "sk-ant-…", text: $key)
                    .settingsField(width: 210, mono: true)
                    .onSubmit { Task { await saveKey() } }
                Button("Save") { Task { await saveKey() } }.disabled(key.trimmingCharacters(in: .whitespaces).isEmpty || checking)
                if replacing { Button("Cancel") { replacing = false; problem = nil } }
            }
        }
    }

    private var modelDetail: String? {
        if models.isEmpty && (saved || !url.isEmpty) { return checking ? "Loading the list…" : "Type a model name, or save a key to choose from a list." }
        if models.isEmpty { return "Save a key first." }
        if !url.isEmpty { return "The models this server offers." }
        return provider == .anthropic ? "Haiku is fast and cheap, and plenty for summaries." : "Luna is OpenAI's fastest and cheapest, and plenty for summaries."
    }

    @ViewBuilder private var modelControl: some View {
        if models.isEmpty {
            TextField(provider == .openAi ? "gpt-6.1-luna" : "claude-haiku-5-5", text: Binding(get: { chosen }, set: setModel))
                .settingsField(width: 210, mono: true)
                .disabled(!saved && url.isEmpty)
        } else {
            Picker("Model", selection: Binding(get: { chosen }, set: setModel)) {
                if !models.contains(where: { $0.id == chosen }) { Text(chosen.isEmpty ? "Choose…" : chosen).tag(chosen) }
                ForEach(models, id: \.id) { Text($0.name).tag($0.id) }
            }
            .settingsMenu()
        }
    }

    private func setModel(_ id: String) {
        model.saveAI { s in if provider == .openAi { s.openaiModel = id } else { s.anthropicModel = id } }
    }

    /// Checks the key by listing the models, and saves it if that works.
    private func saveKey() async {
        let typed = key.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !typed.isEmpty else { return }
        if await loadModels(key: typed) {
            try? model.core.setAiKey(provider: provider, key: typed)
            key = ""
            replacing = false
            model.reloadAI()
        }
    }

    private func saveAddress() {
        let typed = address.trimmingCharacters(in: .whitespaces)
        var settings = model.ai
        if provider == .openAi { settings.openaiUrl = typed } else { settings.anthropicUrl = typed }
        do {
            try model.core.setAiSettings(settings: settings)
        } catch {
            addressProblem = "\(error)"
            return
        }
        addressProblem = nil
        model.reloadAI()
        Task { await loadModels(key: nil) }
    }

    /// Fills the model menu and picks a sensible default. False when the
    /// provider refused or couldn't be reached.
    @discardableResult
    private func loadModels(key: String?) async -> Bool {
        checking = true
        problem = nil
        defer { checking = false }
        do {
            models = try await model.core.aiModels(provider: provider, baseUrl: url, key: key)
            if chosen.isEmpty || !models.contains(where: { $0.id == chosen }), let pick = defaultModel() {
                if chosen.isEmpty { setModel(pick) }
            }
            return true
        } catch {
            problem = "\(error)".replacingOccurrences(of: " Check it in Settings › AI.", with: "")
            return false
        }
    }

    /// The fast, cheap model of the family: Haiku for Anthropic, Luna for
    /// OpenAI (the tiers are Luna, Terra, Sol, Astra), newest first; else
    /// the first listed.
    private func defaultModel() -> String? {
        let ids = models.map(\.id)
        let preferred = provider == .anthropic
            ? ids.first { $0.contains("haiku") }
            : ids.first { $0.hasPrefix("gpt-") && $0.hasSuffix("-luna") } ?? ids.first { $0.hasPrefix("gpt-") && $0.hasSuffix("-mini") }
        return preferred ?? ids.first
    }
}
