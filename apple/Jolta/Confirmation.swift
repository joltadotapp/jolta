import SwiftUI

/// A question before something that can't be undone. Not the system alert:
/// macOS 26 draws an alert's destructive button as red text on a red tint,
/// about 2.5:1, which is hard to read (seen 2026-11-03).
struct Confirmation: Identifiable {
    let title: String
    let message: String
    /// The destructive button's label.
    let action: String
    let perform: () -> Void

    var id: String { title }
}

extension View {
    /// Shows `request` as a sheet until it is answered.
    func confirmation(_ request: Binding<Confirmation?>) -> some View {
        sheet(item: request) { confirmation in
            ConfirmationView(confirmation: confirmation) { request.wrappedValue = nil }
        }
    }
}

private struct ConfirmationView: View {
    let confirmation: Confirmation
    let dismiss: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(confirmation.title)
                .font(.system(size: 15, weight: .semibold))
                .foregroundStyle(Color(Theme.text))
            Text(confirmation.message)
                .font(.system(size: 13))
                .foregroundStyle(Color(Theme.text2))
                .fixedSize(horizontal: false, vertical: true)
            HStack(spacing: 8) {
                Spacer()
                // Return and Esc both cancel, as in the system's own destructive alerts.
                KitButton(title: "Cancel", key: "↩", action: dismiss)
                    .keyboardShortcut(.defaultAction)
                KitButton(title: confirmation.action, key: "⌘⌫", kind: .danger) {
                    dismiss()
                    confirmation.perform()
                }
                .keyboardShortcut(.delete, modifiers: .command)
            }
            .padding(.top, 14)
        }
        .padding(22)
        .frame(width: 400)
        .presentationBackground(Color(Theme.raise))
        .onExitCommand(perform: dismiss)
    }
}
