@preconcurrency import JoltaFFI
import SwiftUI

extension AppModel {
    /// Opens the Add to calendar menu under its link on the card, its
    /// default calendar highlighted.
    func openAddMenu(message: Int64, at frame: CGRect) {
        Task {
            guard let choices = try? await core.addChoices(messageId: message), !choices.isEmpty else {
                notify("Jolta has no calendar it can add this meeting to.")
                return
            }
            addChoices = choices
            addHighlight = (choices.first { $0.isDefault } ?? choices[0]).calendarId
            rsvpPanel = .calendars(frame)
        }
    }

    /// Moves the menu's highlight one calendar up or down.
    func moveAddHighlight(by step: Int) {
        guard let at = addChoices.firstIndex(where: { $0.calendarId == addHighlight }) else { return }
        addHighlight = addChoices[(at + step + addChoices.count) % addChoices.count].calendarId
    }

    /// Puts the card's invitation into a calendar. The card comes back from
    /// the store without the link.
    func addToCalendar(message: Int64, calendar: Int64?) {
        rsvpPanel = nil
        changeCopy(message: message) { core in try await core.addToCalendar(messageId: message, calendarId: calendar) }
    }

    /// Writes the card's newer version over the copy the user added.
    func updateAddedCopy(message: Int64) {
        changeCopy(message: message) { core in try await core.updateAddedCopy(messageId: message) }
    }

    private func changeCopy(message: Int64, _ change: @escaping (Core) async throws -> String) {
        guard pendingRemoval == nil, let threadID = openThread?.threadId else { return }
        pendingRemoval = message
        Task {
            do {
                let text = try await change(core)
                if let view = try? await core.openThread(threadId: threadID, appCss: ReaderStyle.css, utcOffset: utcOffset),
                   selectedThreadID == threadID {
                    openThread = view
                }
                notify(text)
            } catch {
                notify("\(error)")
            }
            pendingRemoval = nil
        }
    }
}

extension RsvpPanel {
    /// Where the panel's left edge goes: under its link, or for the Add to
    /// calendar menu, whose link sits in the card's right corner, ending
    /// under the link's right edge.
    var left: CGFloat {
        switch self {
        case .calendars(let r): r.maxX + 6 - AddCalendarPanel.width
        default: anchor.minX - 6
        }
    }
}

/// The Add to calendar menu: the calendars Jolta can write to, on any
/// account, the default first highlighted. ↑ and ↓ choose, Return adds,
/// Esc closes (keys in `MessageList`).
struct AddCalendarPanel: View {
    static let width: CGFloat = 280
    @Bindable var model: AppModel
    let message: Int64

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text("Add to")
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(Color(Theme.text3))
                .padding(.horizontal, 10)
                .padding(.top, 4)
                .padding(.bottom, 2)
            ForEach(model.addChoices, id: \.calendarId) { choice in
                row(choice)
            }
        }
        .padding(4)
        .frame(width: Self.width)
        .menuPanel()
        .accessibilityIdentifier("add-to-calendar")
    }

    private func row(_ choice: AddChoice) -> some View {
        let on = model.addHighlight == choice.calendarId
        return Button {
            model.addToCalendar(message: message, calendar: choice.calendarId)
        } label: {
            HStack(spacing: 8) {
                Circle()
                    .fill(Color(CalendarColor.color(choice.color, .bar)))
                    .frame(width: 8, height: 8)
                VStack(alignment: .leading, spacing: 0) {
                    Text(choice.name).font(.system(size: Theme.body)).lineLimit(1)
                    Text(choice.account).font(.system(size: 11)).foregroundStyle(Color(Theme.text3)).lineLimit(1)
                }
                Spacer(minLength: 0)
            }
            // Room for the caret of the selected row.
            .padding(.leading, 14)
            .padding(.trailing, 10)
            .frame(height: 36)
            .selectedRow(on)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .foregroundStyle(Color(Theme.text))
        .onHover { if $0 { model.addHighlight = choice.calendarId } }
        .accessibilityLabel("\(choice.name), \(choice.account)")
    }
}
