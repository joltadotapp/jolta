@preconcurrency import JoltaFFI
import SwiftUI

/// Shown when an agent connects for the first time.
struct AgentApproval: View {
    let name: String
    /// Esc: close without an answer.
    let later: () -> Void
    let decide: (Bool, [String]) -> Void
    @State private var read = true
    @State private var organize = false
    @State private var draft = true

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(spacing: 10) {
                Image(systemName: "cpu").font(.system(size: 18)).foregroundStyle(Color(Theme.text2))
                Text("\(name) wants to use your mail").font(.system(size: 15, weight: .semibold))
            }
            Text("Agents connect through Jolta's MCP server. They never see your passwords, everything they do is logged, and they cannot send mail: drafts wait here for you.")
                .font(.system(size: Theme.body))
                .foregroundStyle(Color(Theme.text2))
                .fixedSize(horizontal: false, vertical: true)
            VStack(alignment: .leading, spacing: 8) {
                Toggle("Read and search mail", isOn: $read)
                Toggle("Archive, file and snooze conversations", isOn: $organize)
                Toggle("Write drafts for me to review", isOn: $draft)
            }
            .toggleStyle(.checkbox)
            .font(.system(size: 13))
            HStack(spacing: 8) {
                KitButton(title: "Don't Allow") { decide(false, []) }
                Spacer()
                KitButton(title: "Not Now", key: "esc", action: later)
                    .keyboardShortcut(.cancelAction)
                // No plain Return: a key meant for the mail must not let an agent in.
                KitButton(title: "Allow", key: "⌘↩") {
                    decide(true, [read ? "read" : nil, organize ? "organize" : nil, draft ? "draft" : nil].compactMap { $0 })
                }
                .keyboardShortcut(.return, modifiers: .command)
            }
        }
        .padding(24)
        .frame(width: 460)
        .presentationBackground(Color(Theme.raise))
    }
}

/// Settings › Agents: which agents may do what, and a switch to stop them all.
struct AgentsSettings: View {
    let model: AppModel
    @State private var agents: [AgentInfo] = []
    @State private var paused = false
    /// Why the pause switch, or Revoke or Allow, didn't take.
    @State private var pauseFailure: String?
    @State private var agentFailure: String?

    var body: some View {
        SettingsScroll {
            SettingsSection("Agents", footer: "Agents connect through Jolta's MCP server. They never see your passwords, everything they do is logged under Activity, and they cannot send mail: drafts wait in Jolta for you.") {
                SettingsRow(title: "Pause all agents", detail: pauseFailure ?? "Refuses every agent request until you turn this off.",
                            detailColor: Color(pauseFailure == nil ? Theme.text3 : Theme.dangerText)) {
                    Toggle("Pause all agents", isOn: Binding(get: { paused }, set: pause))
                        .settingsSwitch()
                }
                SettingsRow(title: "Connect an agent", detail: model.mcpStatus) {
                    Button("Copy Path") {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(model.mcpHelperPath, forType: .string)
                    }
                    .help(model.mcpHelperPath)
                }
                SettingsBlock {
                    Text(model.mcpHelperPath)
                        .font(.system(size: 11, design: .monospaced))
                        .foregroundStyle(Color(Theme.text3))
                        .textSelection(.enabled)
                }
            }
            SettingsSection("Connected agents") {
                ForEach(agents, id: \.id) { a in
                    SettingsRow(title: a.name, detail: a.state == "approved"
                                ? "Allowed: \(a.grants.isEmpty ? "nothing" : a.grants.joined(separator: ", "))"
                                : a.state.capitalized) {
                        if a.state == "approved" {
                            Button("Revoke") { decide(a, approve: false, grants: []) }
                        } else {
                            Button("Allow Read and Drafts") { decide(a, approve: true, grants: ["read", "draft"]) }
                        }
                    }
                }
                if let agentFailure {
                    SettingsBlock { Text(agentFailure).foregroundStyle(Color(Theme.dangerText)).font(.system(size: Theme.small)) }
                }
                if agents.isEmpty {
                    SettingsRow("No agents yet", detail: "An agent shows up here the first time it connects, and Jolta asks you to allow it.")
                }
            }
        }
        .onAppear(perform: reload)
        .onChange(of: model.agentRevision) { reload() }
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

    private func decide(_ agent: AgentInfo, approve: Bool, grants: [String]) {
        do {
            try model.core.decideAgent(agentId: agent.id, approve: approve, grants: grants)
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
