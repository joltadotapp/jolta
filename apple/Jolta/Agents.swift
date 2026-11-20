@preconcurrency import JoltaFFI
import SwiftUI

/// Adds an agent, or pairs one again, and shows its MCP configuration once.
/// Jolta keeps only a hash of the token, so it can't show it later.
struct AgentPairing: View {
    struct Request: Identifiable {
        /// Nil adds a new agent.
        let agent: AgentInfo?
        var id: Int64 { agent?.id ?? 0 }
    }

    let model: AppModel
    let request: Request
    let done: () -> Void
    @State private var name = ""
    @State private var read = true
    @State private var organize = false
    @State private var draft = true
    @State private var snippet: String?
    @State private var copied = false
    @State private var failure: String?
    @FocusState private var nameFocused: Bool

    private var title: String { request.agent.map { "Pair \($0.name) again" } ?? "Add an agent" }

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(spacing: 10) {
                Image(systemName: "cpu").font(.system(size: 18)).foregroundStyle(Color(Theme.text2))
                Text(title).font(.system(size: 15, weight: .semibold))
            }
            if let snippet { configuration(snippet) } else { form }
        }
        .padding(24)
        .frame(width: 520)
        .presentationBackground(Color(Theme.raise))
        .onAppear {
            guard let agent = request.agent else { nameFocused = true; return }
            name = agent.name
            if agent.state == "approved" || !agent.paired {
                read = agent.grants.contains("read")
                organize = agent.grants.contains("organize")
                draft = agent.grants.contains("draft")
            }
        }
    }

    private var form: some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Agents connect through Jolta's MCP server with a token Jolta gives them here. They never see your passwords, everything they do is logged, and they cannot send mail: drafts wait here for you.")
                .font(.system(size: Theme.body))
                .foregroundStyle(Color(Theme.text2))
                .fixedSize(horizontal: false, vertical: true)
            if request.agent == nil {
                TextField("Name", text: $name, prompt: Text("Claude Code"))
                    .focused($nameFocused)
            } else {
                Text("Its old token stops working.").font(.system(size: Theme.body)).foregroundStyle(Color(Theme.text2))
            }
            VStack(alignment: .leading, spacing: 8) {
                Toggle("Read and search mail", isOn: $read)
                Toggle("Archive, file and snooze conversations", isOn: $organize)
                Toggle("Write drafts for me to review", isOn: $draft)
            }
            .toggleStyle(.checkbox)
            .font(.system(size: 13))
            if let failure {
                Text(failure).font(.system(size: 12)).foregroundStyle(Color(Theme.dangerText)).fixedSize(horizontal: false, vertical: true)
            }
            HStack(spacing: 8) {
                Spacer()
                KitButton(title: "Cancel", key: "esc", action: done).keyboardShortcut(.cancelAction)
                KitButton(title: request.agent == nil ? "Add Agent" : "Pair Again", key: "↩", kind: .primary) { pair() }
                    .keyboardShortcut(.defaultAction)
                    .disabled(name.trimmingCharacters(in: .whitespaces).isEmpty)
            }
        }
    }

    private func configuration(_ snippet: String) -> some View {
        VStack(alignment: .leading, spacing: 16) {
            Text("Add this to the agent's MCP configuration, then restart the agent. Jolta shows the token only now.")
                .font(.system(size: Theme.body))
                .foregroundStyle(Color(Theme.text2))
                .fixedSize(horizontal: false, vertical: true)
            Text(snippet)
                .font(.system(size: 11, design: .monospaced))
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(12)
                .background(RoundedRectangle(cornerRadius: 6).fill(Color(Theme.bg)))
            HStack(spacing: 8) {
                KitButton(title: copied ? "Copied" : "Copy", key: "⌘C", icon: "doc.on.doc") {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(snippet, forType: .string)
                    copied = true
                }
                .keyboardShortcut("c", modifiers: .command)
                Spacer()
                KitButton(title: "Done", key: "↩", kind: .primary, action: done).keyboardShortcut(.defaultAction)
            }
        }
    }

    private func pair() {
        let grants = [read ? "read" : nil, organize ? "organize" : nil, draft ? "draft" : nil].compactMap { $0 }
        do {
            let token = if let agent = request.agent {
                try model.core.pairAgent(agentId: agent.id, grants: grants)
            } else {
                try model.core.addAgent(name: name, grants: grants)
            }
            snippet = Self.snippet(helper: model.mcpHelperPath, token: token)
            failure = nil
        } catch {
            failure = "\(error)"
        }
        model.agentsChanged()
    }

    /// The common `mcpServers` shape most agents read.
    static func snippet(helper: String, token: String) -> String {
        struct Server: Encodable { let command: String; let env: [String: String] }
        struct Config: Encodable { let mcpServers: [String: Server] }
        let config = Config(mcpServers: ["jolta": Server(command: helper, env: ["JOLTA_MCP_TOKEN": token])])
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
        return (try? encoder.encode(config)).flatMap { String(data: $0, encoding: .utf8) } ?? ""
    }
}

/// The `jolta` command-line tool ships inside the app. The sandbox can't put
/// it on the PATH, so the row shows the command that does and copies it.
private struct CommandLineToolRow: View {
    let command: String
    @State private var copied = false

    /// macOS runs a quarantined app from a temporary copy; a link to it would break.
    private var translocated: Bool { command.contains("/AppTranslocation/") }

    var body: some View {
        SettingsBlock {
            VStack(alignment: .leading, spacing: 10) {
                HStack(spacing: 20) {
                    VStack(alignment: .leading, spacing: 3) {
                        Text("Command-line tool").font(.system(size: 13, weight: .medium))
                        Text(translocated
                             ? "Quit Jolta, move it to Applications and open it again to set this up."
                             : "Run this in Terminal to use jolta there. Anything that runs it, agents included, has your full access: agent permissions, the pause switch and Activity don't apply.")
                            .font(.system(size: 12))
                            .foregroundStyle(Color(Theme.text3))
                            .fixedSize(horizontal: false, vertical: true)
                    }
                    Spacer(minLength: 12)
                    if !translocated {
                        Button(copied ? "Copied" : "Copy") {
                            NSPasteboard.general.clearContents()
                            NSPasteboard.general.setString(command, forType: .string)
                            copied = true
                        }
                    }
                }
                if !translocated { commandText }
            }
        }
    }

    private var commandText: some View {
        Text(command)
            .font(.system(size: 11, design: .monospaced))
            .foregroundStyle(Color(Theme.text2))
            .textSelection(.enabled)
            .fixedSize(horizontal: false, vertical: true)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(10)
            .background(RoundedRectangle(cornerRadius: 6).fill(Color(Theme.bg)))
    }
}

/// Settings › Agents: which agents may do what, and a switch to stop them all.
struct AgentsSettings: View {
    let model: AppModel
    @State private var agents: [AgentInfo] = []
    @State private var paused = false
    /// Why the pause switch, or Revoke, didn't take.
    @State private var pauseFailure: String?
    @State private var agentFailure: String?
    @State private var pairing: AgentPairing.Request?

    var body: some View {
        SettingsScroll {
            SettingsSection("Agents", footer: "Agents connect through Jolta's MCP server. They never see your passwords, everything they do is logged under Activity, and they cannot send mail: drafts wait in Jolta for you.") {
                SettingsRow(title: "Pause all agents", detail: pauseFailure ?? "Refuses every agent request until you turn this off.",
                            detailColor: Color(pauseFailure == nil ? Theme.text3 : Theme.dangerText)) {
                    Toggle("Pause all agents", isOn: Binding(get: { paused }, set: pause))
                        .settingsSwitch()
                }
                SettingsRow(title: "Connect an agent", detail: model.mcpStatus) {
                    Button("Add Agent…") { pairing = .init(agent: nil) }
                }
                CommandLineToolRow(command: model.cliLinkCommand)
            }
            SettingsSection("Connected agents") {
                ForEach(agents, id: \.id) { a in
                    SettingsRow(title: a.name, detail: detail(a),
                                detailColor: Color(a.paired ? Theme.text3 : Theme.warningText)) {
                        if a.paired && a.state == "approved" {
                            Button("Revoke") { revoke(a) }
                        } else {
                            Button("Pair Again…") { pairing = .init(agent: a) }
                        }
                    }
                }
                if let agentFailure {
                    SettingsBlock { Text(agentFailure).foregroundStyle(Color(Theme.dangerText)).font(.system(size: Theme.small)) }
                }
                if agents.isEmpty {
                    SettingsRow("No agents yet", detail: "Add an agent to get the configuration it connects with.")
                }
            }
        }
        .onAppear(perform: reload)
        .onChange(of: model.agentRevision) { reload() }
        .sheet(item: $pairing) { request in
            AgentPairing(model: model, request: request) { pairing = nil }
        }
    }

    private func detail(_ a: AgentInfo) -> String {
        if !a.paired { return "Needs pairing again: Jolta now tells agents apart by a token." }
        if a.state == "approved" { return "Allowed: \(a.grants.isEmpty ? "nothing" : a.grants.joined(separator: ", "))" }
        return "Revoked"
    }

    private func reload() {
        agents = (try? model.core.agents()) ?? []
        paused = (try? model.core.agentsPaused()) ?? false
    }

    /// The switch moves only when the core took the change.
    private func pause(_ on: Bool) {
        do {
            try model.core.setAgentsPaused(paused: on)
            paused = on
            pauseFailure = nil
        } catch {
            pauseFailure = "Jolta couldn't \(on ? "pause" : "resume") the agents: \(error)"
        }
    }

    private func revoke(_ agent: AgentInfo) {
        do {
            try model.core.decideAgent(agentId: agent.id, approve: false, grants: [])
            agentFailure = nil
        } catch {
            agentFailure = "Jolta couldn't change \(agent.name): \(error)"
        }
        reload()
    }
}

/// Settings › Activity: everything agents did, newest first.
struct ActivitySettings: View {
    let model: AppModel
    @State private var log: [AuditInfo] = []

    var body: some View {
        SettingsScroll {
            SettingsSection("Recent activity", footer: "The last 200 requests from agents.") {
                ForEach(Array(log.enumerated()), id: \.offset) { _, e in
                    SettingsBlock {
                        VStack(alignment: .leading, spacing: 3) {
                            HStack {
                                Text(e.agent).fontWeight(.medium)
                                Text(e.tool).foregroundStyle(Color(Theme.text2))
                                Spacer()
                                Text(Date(timeIntervalSince1970: TimeInterval(e.at)).formatted(date: .abbreviated, time: .shortened))
                                    .foregroundStyle(Color(Theme.text3))
                            }
                            Text(e.summary).font(.system(size: 11, design: .monospaced)).foregroundStyle(Color(Theme.text3)).lineLimit(1)
                            if e.outcome != "ok" { Text(e.outcome).font(.system(size: 11)).foregroundStyle(Color(Theme.warningText)).lineLimit(2) }
                        }
                        .font(.system(size: Theme.body))
                    }
                }
                if log.isEmpty {
                    SettingsRow("Nothing yet", detail: "Agent requests appear here as they happen.")
                }
            }
        }
        .onAppear { log = (try? model.core.auditLog(limit: 200)) ?? [] }
        .onChange(of: model.agentRevision) { log = (try? model.core.auditLog(limit: 200)) ?? [] }
    }
}
