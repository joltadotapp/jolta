@preconcurrency import JoltaFFI
import SwiftUI

/// Send an update? after saving a change the guests hear about. The core
/// writes every word and says what each button sends.
struct SendPrompt: Identifiable {
    let dialog: UpdateDialogInfo
    let perform: (NotifyChoice) -> Void

    var id: String { dialog.title }
}

/// "Cancel the meeting for 6 people?": Return and Esc keep the meeting, ⌘⌫
/// cancels it, with the message where the cancellation carries one.
struct CancelPrompt: Identifiable {
    let dialog: CancelDialogInfo
    let perform: (String?) -> Void

    var id: String { dialog.title }
}

/// "Decline and remove?" for a meeting someone else organizes.
struct DeclinePrompt: Identifiable {
    let info: DeclineInfo
    let perform: (_ tell: Bool) -> Void

    var id: String { info.title }
}

extension View {
    func meetingDialogs(send: Binding<SendPrompt?>, cancel: Binding<CancelPrompt?>, decline: Binding<DeclinePrompt?>) -> some View {
        sheet(item: send) { SendPromptView(prompt: $0) { send.wrappedValue = nil } }
            .background(EmptyView().sheet(item: cancel) { CancelPromptView(prompt: $0) { cancel.wrappedValue = nil } })
            .background(EmptyView().sheet(item: decline) { DeclinePromptView(prompt: $0) { decline.wrappedValue = nil } })
    }
}

/// White text on it reads at about 7.7:1, as in `Confirmation`.
private let danger = Color(NSColor(hex: 0xC9302C))

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

    private func choice(_ style: ChoiceStyle) -> SendChoice? { prompt.dialog.choices.first { $0.style == style } }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            DialogText(title: prompt.dialog.title, text: prompt.dialog.body, note: prompt.dialog.note)
            HStack(spacing: 8) {
                if let apart = choice(.apart) {
                    EditorButton(title: apart.label, key: "⌘D") { pick(apart) }
                        .keyboardShortcut("d", modifiers: .command)
                        .accessibilityIdentifier("send-hold")
                }
                Spacer()
                if let back = choice(.back) {
                    EditorButton(title: back.label, key: "Esc", action: dismiss)
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
                        .foregroundStyle(Color(Theme.accent))
                        .accessibilityIdentifier("send-link")
                }
                .padding(.top, 10)
            }
        }
        .padding(22)
        .frame(width: 420)
        .presentationBackground(Color(Theme.raise))
        .onExitCommand(perform: dismiss)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("send-dialog")
    }

    private func pick(_ choice: SendChoice) {
        dismiss()
        if let notify = choice.notify { prompt.perform(notify) }
    }
}

private struct CancelPromptView: View {
    let prompt: CancelPrompt
    let dismiss: () -> Void
    @State private var message = ""

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            DialogText(title: prompt.dialog.title, text: prompt.dialog.body, note: prompt.dialog.note)
            if prompt.dialog.message {
                TextField("Add a message (optional)", text: $message, axis: .vertical)
                    .textFieldStyle(.plain)
                    .lineLimit(2...5)
                    .padding(.vertical, 6)
                    .fieldBox(height: nil)
                    .padding(.top, 12)
                    .accessibilityIdentifier("cancel-message")
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
                    prompt.perform(message.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? nil : message)
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
