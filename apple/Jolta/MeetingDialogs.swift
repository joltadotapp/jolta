@preconcurrency import JoltaFFI
import SwiftUI

/// Send an update? after saving a change the guests hear about. The core
/// writes every word and says what each button sends.
struct SendPrompt: Identifiable {
    let dialog: UpdateDialogInfo
    /// Keep editing or Esc: nothing is saved.
    var back: () -> Void = {}
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

@MainActor private var danger: Color { Color(Theme.danger) }

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
            HStack(spacing: 6) { Text(label); KeyHint(key: "⌘E") }
        }
        .toggleStyle(.checkbox)
        .font(.system(size: 12.5))
        .foregroundStyle(Color(Theme.text2))
        .keyboardShortcut("e", modifiers: .command)
        .padding(.top, 12)
        .accessibilityIdentifier(id)
    }
}

/// A red button with white text and its key.
private struct DangerButton: View {
    let title: String
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 7) { Text(title); KeyHint(key: "⌘⌫") }
                .font(.system(size: 12.5, weight: .semibold))
                .foregroundStyle(.white)
                .padding(.leading, 12)
                .padding(.trailing, 10)
                .frame(height: 28)
                .background(RoundedRectangle(cornerRadius: 7).fill(danger))
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .keyboardShortcut(.delete, modifiers: .command)
    }
}

private struct SendPromptView: View {
    let prompt: SendPrompt
    let dismiss: () -> Void
    @State private var message = ""
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
                    .fieldBox(height: nil)
                    .padding(.top, 12)
                    .accessibilityIdentifier("send-message")
            } else if let label = prompt.dialog.emailGuests {
                EmailGuestsBox(label: label, on: $emailGuests, id: "send-email-guests")
            }
            HStack(spacing: 8) {
                if let apart = choice(.apart) {
                    EditorButton(title: apart.label, key: "⌘D") { pick(apart) }
                        .keyboardShortcut("d", modifiers: .command)
                        .accessibilityIdentifier("send-hold")
                }
                Spacer()
                if let back = choice(.back) {
                    EditorButton(title: back.label, key: "Esc", action: goBack)
                        .keyboardShortcut(.cancelAction)
                        .accessibilityIdentifier("send-back")
                }
                if let primary = choice(.primary) {
                    EditorButton(title: primary.label, key: "↩", primary: true) { pick(primary) }
                        .keyboardShortcut(.defaultAction)
                        .accessibilityIdentifier("send-primary")
                }
            }
            .padding(.top, 16)
            if let link = choice(.link) {
                HStack {
                    Spacer()
                    Button(link.label) { pick(link) }
                        .buttonStyle(.plain)
                        .font(.system(size: 12))
                        .foregroundStyle(Color(Theme.accentText))
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
            .labelsHidden()
            .font(.system(size: 13))
            .padding(.top, 14)
            .accessibilityIdentifier("sender-list")
            HStack(spacing: 8) {
                Spacer()
                EditorButton(title: "Keep editing", key: "Esc", action: goBack)
                    .keyboardShortcut(.cancelAction)
                    .accessibilityIdentifier("sender-back")
                EditorButton(title: "Continue", key: "↩", primary: true) {
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
    @State private var emailGuests = false

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            DialogText(title: prompt.dialog.title, text: prompt.dialog.body, note: nil)
            if prompt.dialog.message {
                TextField("Add a message (optional)", text: $message, axis: .vertical)
                    .textFieldStyle(.plain)
                    .lineLimit(2...5)
                    .padding(.vertical, 6)
                    .fieldBox(height: nil)
                    .padding(.top, 12)
                    .accessibilityIdentifier("cancel-message")
            } else if let label = prompt.dialog.emailGuests {
                EmailGuestsBox(label: label, on: $emailGuests, id: "cancel-email-guests")
            }
            HStack(spacing: 8) {
                Spacer()
                // Return and Esc keep the meeting, as Jolta's other
                // confirmations keep what a destructive button would take.
                EditorButton(title: "Keep meeting", key: "↩", action: dismiss)
                    .keyboardShortcut(.defaultAction)
                    .accessibilityIdentifier("cancel-keep")
                DangerButton(title: "Cancel meeting") {
                    dismiss()
                    prompt.perform(message.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? nil : message, emailGuests)
                }
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
                .font(.system(size: 13))
                .foregroundStyle(Color(Theme.text))
                .padding(.top, 12)
                .accessibilityIdentifier("decline-tell")
            HStack(spacing: 8) {
                Spacer()
                EditorButton(title: "Keep", key: "↩", action: dismiss)
                    .keyboardShortcut(.defaultAction)
                DangerButton(title: "Decline and remove") {
                    dismiss()
                    prompt.perform(tell)
                }
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
