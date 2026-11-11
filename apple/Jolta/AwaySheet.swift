@preconcurrency import JoltaFFI
import AppKit
import SwiftUI

/// What opened the Out of office sheet: an account to start from (the
/// strip's or Settings' Edit), or none (the palette).
struct AwaySheetRequest: Identifiable {
    let id = UUID()
    var accountId: Int64?
}

extension AppModel {
    func openAwaySheet(account: Int64? = nil) {
        palette = nil
        awaySheet = AwaySheetRequest(accountId: account)
    }
}

/// One sheet sets out of office for every ticked account at once and says
/// per account what will happen, since each provider does something else.
/// ⌘Return turns it on.
struct AwaySheet: View {
    let model: AppModel
    let request: AwaySheetRequest
    let close: () -> Void

    @State private var allDay = true
    @State private var from = Date()
    @State private var until = Date()
    @State private var message = ""
    @State private var audience = AwayAudience.everyone
    @State private var declines = AwayDeclines.newInvitations
    @State private var ticked: Set<Int64> = []
    @State private var rows: [AwayRow] = []
    @State private var problem: String?
    @State private var loaded = false
    @State private var working = false
    @State private var failure: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Out of office").font(.system(size: 17, weight: .semibold))
            fields
            accounts
            if let failure {
                Text(failure).font(.system(size: 12)).foregroundStyle(Color(Theme.dangerText)).fixedSize(horizontal: false, vertical: true)
            }
            HStack(spacing: 10) {
                if let problem {
                    Text(problem).font(.system(size: 12)).foregroundStyle(Color(Theme.text3))
                }
                Spacer()
                Button("Cancel", action: close).keyboardShortcut(.cancelAction)
                Button(action: turnOn) {
                    HStack(spacing: 6) {
                        Text("Turn on")
                        Text("⌘↩").foregroundStyle(.secondary)
                    }
                }
                .keyboardShortcut(.return, modifiers: .command)
                .tint(Color(Theme.accent))
                .buttonStyle(.borderedProminent)
                .disabled(!canTurnOn)
                .accessibilityIdentifier("away-turn-on")
            }
        }
        .padding(22)
        .frame(width: 560)
        .background(Color(Theme.bg))
        .foregroundStyle(Color(Theme.text))
        .accessibilityIdentifier("away-sheet")
        .onAppear(perform: load)
        .onChange(of: allDay) { replan() }
        .onChange(of: from) {
            if until < from { until = from.addingTimeInterval(allDay ? 0 : 3600) }
            replan()
        }
        .onChange(of: until) { replan() }
        .onChange(of: audience) { replan() }
        .onChange(of: declines) { replan() }
    }

    private var canTurnOn: Bool {
        loaded && !working && problem == nil && rows.contains { $0.enabled && ticked.contains($0.accountId) }
    }

    private var fields: some View {
        VStack(alignment: .leading, spacing: 8) {
            field("From") {
                DatePicker("From", selection: $from, displayedComponents: allDay ? [.date] : [.date, .hourAndMinute])
                    .labelsHidden()
                    .datePickerStyle(.field)
                Toggle("All day", isOn: $allDay).toggleStyle(.checkbox)
                Spacer()
            }
            field("Until") {
                DatePicker("Until", selection: $until, displayedComponents: allDay ? [.date] : [.date, .hourAndMinute])
                    .labelsHidden()
                    .datePickerStyle(.field)
                if allDay { Text("included").font(.system(size: 12)).foregroundStyle(Color(Theme.text3)) }
                Spacer()
            }
            HStack(alignment: .top, spacing: 12) {
                label("Reply")
                TextEditor(text: $message)
                    .font(.system(size: 13))
                    .scrollContentBackground(.hidden)
                    .padding(6)
                    .frame(height: 84)
                    .background(RoundedRectangle(cornerRadius: 6).fill(Color(Theme.raise)))
                    .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color(Theme.line), lineWidth: 1))
            }
            if tickedRows.contains(where: \.setsReplies) {
                field("Outside") {
                    Picker("Reply outside your organization", selection: $audience) {
                        Text("Everyone gets the reply").tag(AwayAudience.everyone)
                        Text("Only your contacts").tag(AwayAudience.contacts)
                        Text("Nobody outside").tag(AwayAudience.organization)
                    }
                    .labelsHidden()
                    .fixedSize()
                    Spacer()
                }
            }
            if tickedRows.contains(where: \.declinesInvitations) {
                field("Decline") {
                    Picker("What Google declines", selection: $declines) {
                        Text("New invitations").tag(AwayDeclines.newInvitations)
                        Text("New invitations and my meetings").tag(AwayDeclines.everything)
                        Text("Nothing").tag(AwayDeclines.nothing)
                    }
                    .labelsHidden()
                    .fixedSize()
                    Text("on Google").font(.system(size: 12)).foregroundStyle(Color(Theme.text3))
                    Spacer()
                }
            }
        }
    }

    private var tickedRows: [AwayRow] { rows.filter { $0.enabled && ticked.contains($0.accountId) } }

    private func field<Content: View>(_ title: String, @ViewBuilder content: () -> Content) -> some View {
        HStack(spacing: 12) {
            label(title)
            content()
        }
    }

    private func label(_ title: String) -> some View {
        Text(title)
            .font(.system(size: 12.5))
            .foregroundStyle(Color(Theme.text2))
            .frame(width: 56, alignment: .leading)
            .padding(.top, title == "Reply" ? 6 : 0)
    }

    private var accounts: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(rows, id: \.accountId) { row in
                accountRow(row)
                if row.accountId != rows.last?.accountId {
                    Rectangle().fill(Color(Theme.line)).frame(height: 1)
                }
            }
            if loaded && rows.isEmpty {
                Text("No account has a calendar Jolta can write to.")
                    .font(.system(size: 12.5))
                    .foregroundStyle(Color(Theme.text3))
                    .padding(12)
            }
        }
        .background(RoundedRectangle(cornerRadius: 8).fill(Color(Theme.raise)))
        .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Color(Theme.line), lineWidth: 1))
    }

    private func accountRow(_ row: AwayRow) -> some View {
        HStack(alignment: .top, spacing: 10) {
            Toggle("", isOn: Binding(get: { ticked.contains(row.accountId) }, set: { on in
                if on { ticked.insert(row.accountId) } else { ticked.remove(row.accountId) }
            }))
            .toggleStyle(.checkbox)
            .labelsHidden()
            .disabled(!row.enabled)
            .accessibilityIdentifier("away-account-\(row.accountId)")
            VStack(alignment: .leading, spacing: 3) {
                (Text(row.address).fontWeight(.semibold) + Text(" · \(row.provider)").foregroundColor(Color(Theme.text2)))
                    .font(.system(size: 13))
                    .lineLimit(1)
                    .truncationMode(.middle)
                Text(row.line)
                    .font(.system(size: 12))
                    .foregroundStyle(Color(row.enabled ? Theme.text2 : Theme.text3))
                    .fixedSize(horizontal: false, vertical: true)
                HStack(spacing: 12) {
                    if let title = row.linkTitle, let link = row.linkUrl, let url = URL(string: link) {
                        Button("\(title) ↗") { NSWorkspace.shared.open(url) }
                            .buttonStyle(.link)
                            .font(.system(size: 12))
                    }
                    if row.calendarOff {
                        Button("Turn on calendar") {
                            close()
                            model.openSettings(.calendar)
                        }
                        .buttonStyle(.link)
                        .font(.system(size: 12))
                    }
                }
                if let current = row.current {
                    HStack(spacing: 8) {
                        Text(current).font(.system(size: 12, weight: .medium)).foregroundStyle(Color(Theme.accentText))
                        Button("Turn off") { turnOff(row) }
                            .controlSize(.small)
                            .disabled(working)
                            .accessibilityIdentifier("away-turn-off-\(row.accountId)")
                    }
                    .padding(.top, 2)
                }
            }
            Spacer(minLength: 0)
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .opacity(row.enabled ? 1 : 0.75)
    }

    private var current: AwayRequest {
        AwayRequest(accountIds: ticked.sorted(), allDay: allDay, from: Int64(from.timeIntervalSince1970),
                    until: Int64(until.timeIntervalSince1970), message: message, audience: audience, declines: declines)
    }

    private func load() {
        guard !loaded else { return }
        do {
            let draft = try model.core.awayDraft(accountId: request.accountId)
            allDay = draft.allDay
            from = Date(timeIntervalSince1970: TimeInterval(draft.from))
            until = Date(timeIntervalSince1970: TimeInterval(draft.until))
            message = draft.message
            audience = draft.audience
            declines = draft.declines
            let plan = try model.core.awayPlan(request: draft)
            rows = plan.rows
            problem = plan.problem
            ticked = draft.accountIds.isEmpty ? Set(plan.rows.filter(\.ticked).map(\.accountId)) : Set(draft.accountIds)
        } catch {
            failure = "\(error)"
        }
        loaded = true
    }

    private func replan() {
        guard loaded else { return }
        do {
            let plan = try model.core.awayPlan(request: current)
            rows = plan.rows
            problem = plan.problem
        } catch {
            failure = "\(error)"
        }
    }

    private func turnOn() {
        guard canTurnOn else { return }
        working = true
        let request = current
        Task {
            do {
                let toast = try await model.core.setAway(request: request)
                model.notify(toast)
                model.reloadAway()
                close()
            } catch {
                failure = "\(error)"
            }
            working = false
        }
    }

    private func turnOff(_ row: AwayRow) {
        working = true
        Task {
            do {
                model.notify(try await model.core.endAway(accountIds: [row.accountId]))
                model.reloadAway()
                replan()
            } catch {
                failure = "\(error)"
            }
            working = false
        }
    }
}
