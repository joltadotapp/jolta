@preconcurrency import JoltaFFI
import SwiftUI

/// Shown when an agent connects for the first time.
struct AgentApproval: View {
    let name: String
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
                .font(.system(size: 12.5))
                .foregroundStyle(Color(Theme.text2))
                .fixedSize(horizontal: false, vertical: true)
            VStack(alignment: .leading, spacing: 8) {
                Toggle("Read and search mail", isOn: $read)
                Toggle("Archive, file and snooze conversations", isOn: $organize)
                Toggle("Write drafts for me to review", isOn: $draft)
            }
            .toggleStyle(.checkbox)
            .font(.system(size: 13))
            HStack {
                Button("Don't allow") { decide(false, []) }
                    .keyboardShortcut(.cancelAction)
                Spacer()
                Button("Allow") {
                    decide(true, [read ? "read" : nil, organize ? "organize" : nil, draft ? "draft" : nil].compactMap { $0 })
                }
                .keyboardShortcut(.defaultAction)
            }
        }
        .padding(24)
        .frame(width: 460)
        .background(RoundedRectangle(cornerRadius: 14).fill(Color(Theme.raise)))
        .shadow(color: .black.opacity(0.45), radius: 32, y: 16)
    }
}

/// Settings › Agents: which agents may do what, and a switch to stop them all.
struct AgentsSettings: View {
    let model: AppModel
    @State private var agents: [AgentInfo] = []
    @State private var paused = false

    var body: some View {
        SettingsScroll {
            SettingsSection("Agents", footer: "Agents connect through Jolta's MCP server. They never see your passwords, everything they do is logged under Activity, and they cannot send mail: drafts wait in Jolta for you.") {
                SettingsRow(title: "Pause all agents", detail: "Refuses every agent request until you turn this off.") {
                    Toggle("Pause all agents", isOn: $paused)
                        .settingsSwitch()
                        .onChange(of: paused) { _, on in try? model.core.setAgentsPaused(paused: on) }
                }
                SettingsRow(title: "Connect an agent", detail: model.mcpStatus) {
                    Button("Copy Command") {
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
                            Button("Revoke") { try? model.core.decideAgent(agentId: a.id, approve: false, grants: []); reload() }
                        } else {
                            Button("Allow Read and Drafts") { try? model.core.decideAgent(agentId: a.id, approve: true, grants: ["read", "draft"]); reload() }
                        }
                    }
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
                            if e.outcome != "ok" { Text(e.outcome).font(.system(size: 11)).foregroundStyle(.orange).lineLimit(2) }
                        }
                        .font(.system(size: 12.5))
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
