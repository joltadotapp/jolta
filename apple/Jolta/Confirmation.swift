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

    /// White text on it reads at about 7.7:1.
    private static let danger = Color(NSColor(hex: 0xC9302C))

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
                Button("Cancel", action: dismiss)
                    .buttonStyle(DialogButton(fill: Color(Theme.kbd), text: Color(Theme.text), border: Color(Theme.line)))
                    .keyboardShortcut(.defaultAction)
                Button(confirmation.action) {
                    dismiss()
                    confirmation.perform()
                }
                .buttonStyle(DialogButton(fill: Self.danger, text: .white))
            }
            .padding(.top, 14)
        }
        .padding(22)
        .frame(width: 400)
        .presentationBackground(Color(Theme.raise))
        .onExitCommand(perform: dismiss)
    }
}

struct DialogButton: ButtonStyle {
    let fill: Color
    let text: Color
    var border: Color = .clear

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: 13, weight: .medium))
            .foregroundStyle(text)
            .padding(.horizontal, 14)
            .frame(height: 30)
            .background(RoundedRectangle(cornerRadius: 8).fill(fill).strokeBorder(border, lineWidth: 1))
            .opacity(configuration.isPressed ? 0.8 : 1)
            .contentShape(Rectangle())
    }
}
