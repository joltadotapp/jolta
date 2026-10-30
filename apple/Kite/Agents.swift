@preconcurrency import KiteFFI
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
            Text("Agents connect through Kite's MCP server. They never see your passwords, everything they do is logged, and they cannot send mail: drafts wait here for you.")
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

/// Settings: which agents may do what, what they did, and a switch to stop them all.
struct AgentsSettings: View {
    let model: AppModel?
    @State private var agents: [AgentInfo] = []
    @State private var log: [AuditInfo] = []
    @State private var paused = false

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Toggle("Pause all agents", isOn: $paused)
                .toggleStyle(.switch)
                .onChange(of: paused) { _, on in try? model?.core.setAgentsPaused(paused: on) }
            if let model {
                Text("Agent config: command \(model.mcpHelperPath)").font(.system(size: 11, design: .monospaced)).textSelection(.enabled)
                Text(model.mcpStatus).font(.system(size: 11)).foregroundStyle(.secondary)
            }
            Text("Agents").font(.system(size: 13, weight: .semibold)).padding(.top, 6)
            ForEach(agents, id: \.id) { a in
                HStack {
                    Text(a.name)
                    Text(a.state).foregroundStyle(.secondary)
                    Text(a.grants.joined(separator: ", ")).foregroundStyle(.secondary).font(.system(size: 11))
                    Spacer()
                    if a.state == "approved" {
                        Button("Revoke") { try? model?.core.decideAgent(agentId: a.id, approve: false, grants: []); reload() }
                    } else {
                        Button("Allow read and drafts") { try? model?.core.decideAgent(agentId: a.id, approve: true, grants: ["read", "draft"]); reload() }
                    }
                }
                .font(.system(size: 12.5))
            }
            Text("Activity").font(.system(size: 13, weight: .semibold)).padding(.top, 6)
            List(Array(log.enumerated()), id: \.offset) { _, e in
                VStack(alignment: .leading, spacing: 2) {
                    HStack {
                        Text(e.agent).fontWeight(.medium)
                        Text(e.tool)
                        Spacer()
                        Text(Date(timeIntervalSince1970: TimeInterval(e.at)).formatted(date: .abbreviated, time: .shortened))
                            .foregroundStyle(.secondary)
                    }
                    Text(e.summary).font(.system(size: 11, design: .monospaced)).foregroundStyle(.secondary).lineLimit(1)
                    if e.outcome != "ok" { Text(e.outcome).font(.system(size: 11)).foregroundStyle(.orange).lineLimit(2) }
                }
                .font(.system(size: 12))
            }
            .frame(minHeight: 200)
        }
        .padding(20)
        .onAppear(perform: reload)
        .onChange(of: model?.agentRevision) { reload() }
    }

    private func reload() {
        agents = (try? model?.core.agents()) ?? []
        log = (try? model?.core.auditLog(limit: 200)) ?? []
        paused = (try? model?.core.agentsPaused()) ?? false
    }
}
