@preconcurrency import JoltaFFI
import SwiftUI

/// Send an update? after saving a change the guests hear about. The core
/// writes every word and says what each button sends.
struct SendPrompt: Identifiable {
    let dialog: UpdateDialogInfo
    /// Keep editing or Esc: nothing is saved.
    var back: () -> Void = {}
    /// What the back button says where there is no editor to go back to.
    var backTitle: String?
    /// The chosen button's notify, the message where the dialog has a
    /// field, and whether Email guests opens once it is saved.
    let perform: (NotifyChoice, String?, Bool) -> Void

    var id: String { dialog.title }
}

/// "Send invitations from": Jolta mails the guests of a calendar on this
/// Mac itself, from the account picked here, asked once per calendar.
struct SenderPrompt: Identifiable {
    let calendar: String
    let senders: [SenderInfo]
    /// Keep editing or Esc: nothing is saved.
    var cancel: () -> Void = {}
    var backTitle = "Keep editing"
    let perform: (SenderInfo) -> Void

    var id: String { calendar }
}

/// "Cancel the meeting for 6 people?": Return and Esc keep the meeting, ⌘⌫
/// cancels it, with the message where the cancellation carries one.
struct CancelPrompt: Identifiable {
    let dialog: CancelDialogInfo
    /// The message where the cancellation carries one, and whether Email
    /// guests opens once the meeting is cancelled.
    let perform: (String?, Bool) -> Void

    var id: String { dialog.title }
}

/// "Decline and remove?" for a meeting someone else organizes.
struct DeclinePrompt: Identifiable {
    let info: DeclineInfo
    let perform: (_ tell: Bool) -> Void

    var id: String { info.title }
}

extension View {
    func meetingDialogs(send: Binding<SendPrompt?>, cancel: Binding<CancelPrompt?>, decline: Binding<DeclinePrompt?>, sender: Binding<SenderPrompt?>) -> some View {
        sheet(item: send) { SendPromptView(prompt: $0) { send.wrappedValue = nil } }
            .background(EmptyView().sheet(item: cancel) { CancelPromptView(prompt: $0) { cancel.wrappedValue = nil } })
            .background(EmptyView().sheet(item: decline) { DeclinePromptView(prompt: $0) { decline.wrappedValue = nil } })
            .background(EmptyView().sheet(item: sender) { SenderPromptView(prompt: $0) { sender.wrappedValue = nil } })
    }
}

private struct DialogText: View {
    let title: String
    let text: String
    let note: String?

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title).font(.system(size: 15, weight: .semibold)).foregroundStyle(Color(Theme.text))
            Text(text).font(.system(size: 13)).foregroundStyle(Color(Theme.text2)).fixedSize(horizontal: false, vertical: true)
            if let note {
                Text(note).font(.system(size: 12)).foregroundStyle(Color(Theme.text3)).fixedSize(horizontal: false, vertical: true)
            }
        }
    }
}

/// Where a server's update or cancellation carries no words of the user's:
/// a box that opens Email guests once it is done. ⌘E ticks it.
private struct EmailGuestsBox: View {
    let label: String
    @Binding var on: Bool
    let id: String

    var body: some View {
        Toggle(isOn: $on) {
            HStack(spacing: 6) { Text(label); KeyCap(key: "⌘E", style: .inherit) }
        }
        .toggleStyle(.checkbox)
        .tint(Color(Theme.accent))
        .font(.system(size: Theme.body))
        .foregroundStyle(Color(Theme.text2))
        .keyboardShortcut("e", modifiers: .command)
        .padding(.top, 12)
        .accessibilityIdentifier(id)
    }
}

private struct SendPromptView: View {
    let prompt: SendPrompt
    let dismiss: () -> Void
    @State private var message = ""
    @FocusState private var writing: Bool
    @State private var emailGuests = false

    private func choice(_ style: ChoiceStyle) -> SendChoice? { prompt.dialog.choices.first { $0.style == style } }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            DialogText(title: prompt.dialog.title, text: prompt.dialog.body, note: nil)
            if prompt.dialog.message {
                TextField("Add a message (optional)", text: $message, axis: .vertical)
                    .textFieldStyle(.plain)
                    .lineLimit(2...5)
                    .padding(.vertical, 6)
                    .fieldBox(focused: writing, height: nil)
                    .focused($writing)
                    .padding(.top, 12)
                    .accessibilityIdentifier("send-message")
            } else if let label = prompt.dialog.emailGuests {
                EmailGuestsBox(label: label, on: $emailGuests, id: "send-email-guests")
            }
            HStack(spacing: 8) {
                if let apart = choice(.apart) {
                    KitButton(title: apart.label, key: "⌘D") { pick(apart) }
                        .keyboardShortcut("d", modifiers: .command)
                        .accessibilityIdentifier("send-hold")
                }
                Spacer()
                if let back = choice(.back) {
                    KitButton(title: prompt.backTitle ?? back.label, key: "esc", action: goBack)
                        .keyboardShortcut(.cancelAction)
                        .accessibilityIdentifier("send-back")
                }
                if let primary = choice(.primary) {
                    KitButton(title: primary.label, key: "↩", kind: .primary) { pick(primary) }
                        .keyboardShortcut(.defaultAction)
                        .accessibilityIdentifier("send-primary")
                }
            }
            .padding(.top, 16)
            if let link = choice(.link) {
                HStack {
                    Spacer()
                    KitButton(title: link.label, kind: .quiet, size: .small) { pick(link) }
                        .accessibilityIdentifier("send-link")
                }
                .padding(.top, 10)
            }
        }
        .padding(22)
        .frame(width: 420)
        .presentationBackground(Color(Theme.raise))
        .onExitCommand(perform: goBack)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("send-dialog")
    }

    private func goBack() {
        dismiss()
        prompt.back()
    }

    private func pick(_ choice: SendChoice) {
        dismiss()
        let words = message.trimmingCharacters(in: .whitespacesAndNewlines)
        if let notify = choice.notify { prompt.perform(notify, words.isEmpty ? nil : words, emailGuests && notify != NotifyChoice.none) } else { prompt.back() }
    }
}

private struct SenderPromptView: View {
    let prompt: SenderPrompt
    let dismiss: () -> Void
    @State private var picked: Int64?

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            DialogText(title: "Send invitations from",
                       text: "Jolta emails the guests of \(prompt.calendar) itself. Their invitations, updates and cancellations come from this account, and their answers come back to it.",
                       note: "You can change it later in Settings › Calendar.")
            Picker("Account", selection: Binding(get: { picked ?? prompt.senders.first?.accountId }, set: { picked = $0 })) {
                ForEach(prompt.senders, id: \.accountId) { sender in
                    Text(sender.name.isEmpty ? sender.address : "\(sender.name) · \(sender.address)").tag(Optional(sender.accountId))
                }
            }
            .pickerStyle(.radioGroup)
            .tint(Color(Theme.accent))
            .labelsHidden()
            .font(.system(size: 13))
            .padding(.top, 14)
            .accessibilityIdentifier("sender-list")
            HStack(spacing: 8) {
                Spacer()
                KitButton(title: prompt.backTitle, key: "esc", action: goBack)
                    .keyboardShortcut(.cancelAction)
                    .accessibilityIdentifier("sender-back")
                KitButton(title: "Continue", key: "↩", kind: .primary) {
                    let id = picked ?? prompt.senders.first?.accountId
                    dismiss()
                    if let sender = prompt.senders.first(where: { $0.accountId == id }) { prompt.perform(sender) }
                }
                .keyboardShortcut(.defaultAction)
                .accessibilityIdentifier("sender-continue")
            }
            .padding(.top, 16)
        }
        .padding(22)
        .frame(width: 420)
        .presentationBackground(Color(Theme.raise))
        .onExitCommand(perform: goBack)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("sender-dialog")
    }

    private func goBack() {
        dismiss()
        prompt.cancel()
    }
}

private struct CancelPromptView: View {
    let prompt: CancelPrompt
    let dismiss: () -> Void
    @State private var message = ""
    @FocusState private var writing: Bool
    @State private var emailGuests = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            DialogText(title: prompt.dialog.title, text: prompt.dialog.body, note: nil)
            if prompt.dialog.message {
                TextField("Add a message (optional)", text: $message, axis: .vertical)
                    .textFieldStyle(.plain)
                    .lineLimit(2...5)
                    .padding(.vertical, 6)
                    .fieldBox(focused: writing, height: nil)
                    .focused($writing)
                    .padding(.top, 12)
                    .accessibilityIdentifier("cancel-message")
            } else if let label = prompt.dialog.emailGuests {
                EmailGuestsBox(label: label, on: $emailGuests, id: "cancel-email-guests")
            }
            HStack(spacing: 8) {
                Spacer()
                // Return and Esc keep the meeting, as Jolta's other
                // confirmations keep what a destructive button would take.
                KitButton(title: "Keep meeting", key: "↩", action: dismiss)
                    .keyboardShortcut(.defaultAction)
                    .accessibilityIdentifier("cancel-keep")
                KitButton(title: "Cancel meeting", key: "⌘⌫", kind: .danger) {
                    dismiss()
                    prompt.perform(message.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? nil : message, emailGuests)
                }
                .keyboardShortcut(.delete, modifiers: .command)
                .accessibilityIdentifier("cancel-meeting")
            }
            .padding(.top, 16)
        }
        .padding(22)
        .frame(width: 400)
        .presentationBackground(Color(Theme.raise))
        .onExitCommand(perform: dismiss)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("cancel-dialog")
    }
}

private struct DeclinePromptView: View {
    let prompt: DeclinePrompt
    let dismiss: () -> Void
    @State private var tell = true

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            DialogText(title: prompt.info.title, text: prompt.info.body, note: nil)
            Toggle("Tell \(prompt.info.organizer)", isOn: $tell)
                .toggleStyle(.checkbox)
                .tint(Color(Theme.accent))
                .font(.system(size: 13))
                .foregroundStyle(Color(Theme.text))
                .padding(.top, 12)
                .accessibilityIdentifier("decline-tell")
            HStack(spacing: 8) {
                Spacer()
                KitButton(title: "Keep", key: "↩", action: dismiss)
                    .keyboardShortcut(.defaultAction)
                KitButton(title: "Decline and remove", key: "⌘⌫", kind: .danger) {
                    dismiss()
                    prompt.perform(tell)
                }
                .keyboardShortcut(.delete, modifiers: .command)
                .accessibilityIdentifier("decline-remove")
            }
            .padding(.top, 16)
        }
        .padding(22)
        .frame(width: 400)
        .presentationBackground(Color(Theme.raise))
        .onExitCommand(perform: dismiss)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("decline-dialog")
    }
}
