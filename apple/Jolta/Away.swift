@preconcurrency import JoltaFFI
import AppKit
import SwiftUI

/// The line at the top of the mail list while an account's automatic
/// replies are on: forgetting them is the common failure.
struct AwayStrip: View {
    let info: AwayInfo
    let model: AppModel

    var body: some View {
        HStack(spacing: 10) {
            RoundedRectangle(cornerRadius: 2).fill(Color(Theme.accent)).frame(width: 8, height: 8)
            // A narrow list drops the end before the account, then shortens.
            ViewThatFits(in: .horizontal) {
                sentence(until: true).fixedSize()
                sentence(until: false).fixedSize()
                sentence(until: true).lineLimit(1)
            }
            .layoutPriority(1)
            .help("Automatic replies are on for \(info.address)" + (info.untilText.map { " until \($0)" } ?? ""))
            Spacer(minLength: 6)
            StripButton(title: "Edit") { model.openAwaySettings(info) }
            StripButton(title: "Turn off") { model.turnOffAway(info) }
                .disabled(model.turningOffAway.contains(info.accountId))
        }
        .font(.system(size: 12.5))
        .foregroundStyle(Color(Theme.text))
        .padding(.horizontal, 12)
        .padding(.vertical, 7)
        .background(RoundedRectangle(cornerRadius: 10).fill(Color(Theme.raise)))
        .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Color(Theme.line), lineWidth: 1))
        .padding(.horizontal, 14)
        .padding(.top, 8)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("away-strip")
    }

    private func sentence(until: Bool) -> Text {
        Text("Automatic replies are on").fontWeight(.semibold)
            + Text(" for \(info.address)")
            + Text(until ? info.untilText.map { " until \($0)" } ?? "" : "")
    }
}

private struct StripButton: View {
    let title: String
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Text(title)
                .font(.system(size: 12, weight: .semibold))
                .padding(.horizontal, 10)
                .frame(height: 24)
                .background(RoundedRectangle(cornerRadius: 6).fill(Color(Theme.fill)))
                .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color(Theme.line), lineWidth: 1))
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .fixedSize()
    }
}

/// Settings › Calendar › Out of office: each Google and Microsoft 365
/// account's automatic replies as the provider last said.
struct OutOfOfficeRows: View {
    let model: AppModel
    let switches: CalendarSwitches?
    @State private var rows: [AwayInfo] = []

    var body: some View {
        ForEach(rows, id: \.accountId) { row in
            SettingsRow(title: row.address, detail: row.detail) {
                switch row.state {
                case .on:
                    Button("Turn off") { model.turnOffAway(row) }
                        .disabled(model.turningOffAway.contains(row.accountId))
                case .calendarOff:
                    if let switches, let account = model.calendar.list.accounts.first(where: { $0.account.id == row.accountId }) {
                        Button("Turn on calendar") { switches.set(account, on: true) }
                    }
                default:
                    Button("Open settings") { model.openAwaySettings(row) }
                }
            }
        }
        if rows.isEmpty {
            SettingsRow("No Google or Microsoft 365 accounts", detail: "Jolta reads automatic replies from Gmail and Microsoft 365.")
        }
        Color.clear.frame(height: 0)
            .onChange(of: model.awayRevision, initial: true) { reload() }
            .onAppear { model.core.checkAway() }
    }

    private func reload() {
        rows = (try? model.core.awayAccounts()) ?? []
    }
}
