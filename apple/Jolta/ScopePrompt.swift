@preconcurrency import JoltaFFI
import SwiftUI

/// "Change a repeating event": which events of a series a save or delete is
/// for. The core lists the choices in order, the first the default.
struct ScopePrompt: Identifiable {
    let title: String
    /// "You moved Standup to 10:00."
    let line: String?
    let choices: [ScopeChoice]
    /// Cancel or Esc: nothing is saved.
    var cancel: () -> Void = {}
    let perform: (EditScope) -> Void

    var id: String { title }
}

extension View {
    /// Shows `request` as a sheet until a row is picked or it is cancelled.
    func scopePrompt(_ request: Binding<ScopePrompt?>) -> some View {
        sheet(item: request) { prompt in
            ScopePromptView(prompt: prompt, dismiss: { request.wrappedValue = nil }) {
                request.wrappedValue = nil
                prompt.cancel()
            }
        }
    }
}

/// Number keys pick a row, Return the first, Esc cancels.
private struct ScopePromptView: View {
    let prompt: ScopePrompt
    let dismiss: () -> Void
    let cancel: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text(prompt.title)
                .font(.system(size: 15, weight: .semibold))
                .foregroundStyle(Color(Theme.text))
            if let line = prompt.line {
                Text(line)
                    .font(.system(size: 13))
                    .foregroundStyle(Color(Theme.text2))
                    .fixedSize(horizontal: false, vertical: true)
                    .padding(.top, 8)
            }
            VStack(spacing: 0) {
                ForEach(Array(prompt.choices.enumerated()), id: \.offset) { index, choice in
                    row(choice, index: index)
                }
            }
            .padding(.top, 12)
            HStack {
                Spacer()
                KitButton(title: "Cancel", key: "esc", action: cancel)
                    .keyboardShortcut(.cancelAction)
            }
            .background {
                // A row button holds its number key, so Return needs a button of its own.
                if let first = prompt.choices.first {
                    Button("") { pick(first) }.keyboardShortcut(.defaultAction).opacity(0).allowsHitTesting(false).accessibilityHidden(true)
                }
            }
            .padding(.top, 14)
        }
        .padding(.horizontal, 22)
        .padding(.vertical, 20)
        .frame(width: 380)
        .presentationBackground(Color(Theme.raise))
        .onExitCommand(perform: cancel)
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("scope-prompt")
    }

    private func pick(_ choice: ScopeChoice) {
        dismiss()
        prompt.perform(choice.scope)
    }

    private func row(_ choice: ScopeChoice, index: Int) -> some View {
        let first = index == 0
        return Button { pick(choice) } label: {
            HStack(alignment: .top, spacing: 11) {
                KeyCap(key: "\(index + 1)", focused: first).frame(width: 17)
                VStack(alignment: .leading, spacing: 2) {
                    Text(choice.label).font(.system(size: 13, weight: .medium)).foregroundStyle(Color(Theme.text))
                    if let detail = choice.detail {
                        Text(detail).font(.system(size: Theme.small)).foregroundStyle(Color(Theme.text3)).fixedSize(horizontal: false, vertical: true)
                    }
                }
                Spacer(minLength: 0)
                if first { KeyCap(key: "↩", focused: true) }
            }
            .padding(.horizontal, 10)
            .padding(.vertical, 8)
            .selectedRow(first)
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .padding(.horizontal, -10)
        .keyboardShortcut(KeyEquivalent(Character("\(index + 1)")), modifiers: [])
        .accessibilityIdentifier("scope-\(index + 1)")
    }
}
