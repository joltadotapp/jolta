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
            RoundedRectangle(cornerRadius: 2).fill(Color(Theme.text2)).frame(width: 8, height: 8)
            // A narrow list drops the end before the account, then shortens.
            ViewThatFits(in: .horizontal) {
                sentence(until: true).fixedSize()
                sentence(until: false).fixedSize()
                sentence(until: true).lineLimit(1)
            }
            .layoutPriority(1)
            .help("Automatic replies are on for \(info.address)" + (info.untilText.map { " until \($0)" } ?? ""))
            Spacer(minLength: 6)
            if info.editable {
                KitButton(title: "Edit", size: .small) { model.openAwaySheet(account: info.accountId) }
                KitButton(title: "Turn Off", size: .small) { model.turnOffAway(info) }
                    .disabled(model.turningOffAway.contains(info.accountId))
            } else {
                KitButton(title: info.settingsTitle, size: .small) { model.openAwaySettings(info) }
            }
        }
        .font(.system(size: Theme.body))
        .foregroundStyle(Color(Theme.text))
        .padding(.horizontal, 12)
        .padding(.vertical, 7)
        .card()
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

/// Settings › Calendar › Out of office: each Google and Microsoft 365
/// account's automatic replies as the provider last said.
struct OutOfOfficeRows: View {
    let model: AppModel
    let switches: CalendarSwitches?
    @State private var rows: [AwayInfo] = []
    /// The sheet opens in the settings window, over this page.
    @State private var sheet: AwaySheetRequest?

    var body: some View {
        SettingsRow(title: "Set out of office", detail: "Automatic replies on Microsoft 365 and an out-of-office event in each calendar, for the days you pick.") {
            Button("Set…") { sheet = AwaySheetRequest() }
                .accessibilityIdentifier("away-set")
        }
        .sheet(item: $sheet) { request in
            AwaySheet(model: model, request: request) { sheet = nil }
        }
        ForEach(rows, id: \.accountId) { row in
            SettingsRow(title: row.address, detail: row.detail) {
                switch row.state {
                case .calendarOff:
                    if let switches, let account = model.calendar.list.accounts.first(where: { $0.account.id == row.accountId }) {
                        Button("Turn On Calendar") { switches.set(account, on: true) }
                    }
                case _ where !row.editable:
                    Button(row.settingsTitle) { model.openAwaySettings(row) }
                case .on:
                    Button("Turn Off") { model.turnOffAway(row) }
                        .disabled(model.turningOffAway.contains(row.accountId))
                default:
                    Button("Edit…") { sheet = AwaySheetRequest(accountId: row.accountId) }
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
