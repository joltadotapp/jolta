@preconcurrency import KiteFFI
import SwiftUI
import UniformTypeIdentifiers

/// Writing a message, in place of the reader. ⌘↩ sends (after the undo
/// window), Esc closes and keeps a draft.
struct ComposeView: View {
    @Bindable var model: AppModel
    @State private var showCc = false
    @FocusState private var focus: Field?
    @State private var saveTask: Task<Void, Never>?
    @State private var selection: TextSelection?

    enum Field { case to, cc, bcc, subject, body }

    private var draft: Binding<ComposeData> {
        // A field that commits as the view closes must not reopen a discarded or sent message.
        Binding(get: { model.compose ?? ComposeData.blank }, set: {
            guard model.compose != nil else { return }
            model.compose = $0
            scheduleSave()
        })
    }

    private var senders: [AccountInfo] {
        model.accounts.map(\.account).filter { $0.provider != "Synthetic" }
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 4) {
                Text(draft.wrappedValue.subject.isEmpty ? "New message" : draft.wrappedValue.subject)
                    .font(.system(size: 13, weight: .semibold))
                    .lineLimit(1)
                Spacer()
                ActionButton(icon: "paperclip", title: "Attach", key: "⇧⌘A") { attach() }
                    .keyboardShortcut("a", modifiers: [.command, .shift])
                ActionButton(icon: "trash", title: "Discard", key: "⌘⌫") { model.discardCompose() }
                    .keyboardShortcut(.delete, modifiers: .command)
                SendButton { model.sendCompose() }
                    .keyboardShortcut(.return, modifiers: .command)
            }
            .padding(.leading, 28)
            .padding(.trailing, 16)
            .frame(height: Theme.headerHeight)
            .overlay(alignment: .bottom) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }

            VStack(spacing: 0) {
                if senders.count > 1 {
                    FieldRow(label: "From") {
                        Picker("", selection: draft.accountId) {
                            ForEach(senders, id: \.id) { Text($0.address).tag(Optional($0.id)) }
                        }
                        .labelsHidden()
                        .pickerStyle(.menu)
                        .fixedSize()
                    }
                }
                FieldRow(label: "To") {
                    recipientField(draft.to, .to)
                    if !showCc {
                        Button("Cc Bcc") { showCc = true; focus = .cc }
                            .buttonStyle(.plain)
                            .font(.system(size: 12))
                            .foregroundStyle(Color(Theme.text3))
                    }
                }
                .zIndex(focus == .to ? 1 : 0)
                if showCc {
                    FieldRow(label: "Cc") { recipientField(draft.cc, .cc) }
                        .zIndex(focus == .cc ? 1 : 0)
                    FieldRow(label: "Bcc") { recipientField(draft.bcc, .bcc) }
                        .zIndex(focus == .bcc ? 1 : 0)
                }
                FieldRow(label: "Subject") { TextField("", text: draft.subject).focused($focus, equals: .subject) }
            }
            .padding(.horizontal, 28)
            .textFieldStyle(.plain)
            // Suggestions hang over the subject and the body.
            .zIndex(1)

            TextEditor(text: draft.body, selection: $selection)
                .font(.system(size: 14))
                .lineSpacing(5)
                .scrollContentBackground(.hidden)
                .focused($focus, equals: .body)
                .padding(.horizontal, 23)
                .padding(.top, 14)

            if !draft.wrappedValue.attachments.isEmpty {
                ScrollView(.horizontal) {
                    HStack(spacing: 8) {
                        ForEach(Array(draft.wrappedValue.attachments.enumerated()), id: \.offset) { i, file in
                            HStack(spacing: 8) {
                                Image(systemName: "doc").foregroundStyle(Color(Theme.text3))
                                Text(file.name).lineLimit(1)
                                Button { model.compose?.attachments.remove(at: i); scheduleSave() } label: {
                                    Image(systemName: "xmark").font(.system(size: 10, weight: .semibold))
                                }
                                .buttonStyle(.plain)
                                .foregroundStyle(Color(Theme.text3))
                            }
                            .font(.system(size: 12.5))
                            .padding(.horizontal, 12)
                            .frame(height: 34)
                            .overlay(RoundedRectangle(cornerRadius: 8).strokeBorder(Color(Theme.line), lineWidth: 1))
                        }
                    }
                    .padding(.horizontal, 28)
                }
                .scrollIndicators(.never)
                .padding(.bottom, 16)
            }
        }
        .background(Color(Theme.reader))
        .onAppear {
            // The first suggestion reads all sent mail; do it before the first keystroke.
            let core = model.core
            Task.detached(priority: .utility) { _ = try? core.suggestAddresses(typed: "", accountId: nil, recipients: "") }
            showCc = !(model.compose?.cc.isEmpty ?? true) || !(model.compose?.bcc.isEmpty ?? true)
            focus = (model.compose?.to.isEmpty ?? true) ? .to : .body
            // Replies and forwards start with the quote below; write above it.
            if let body = model.compose?.body {
                selection = TextSelection(insertionPoint: body.startIndex)
            }
        }
        .onExitCommand { model.closeCompose() }
        .onDisappear { saveTask?.cancel() }
    }

    private func recipientField(_ text: Binding<String>, _ field: Field) -> some View {
        let d = draft.wrappedValue
        return RecipientField(text: text, core: model.core, account: d.accountId,
                              recipients: [d.to, d.cc, d.bcc].joined(separator: ", "), active: focus == field)
            .focused($focus, equals: field)
    }

    /// Saves the draft a few seconds after the last edit.
    private func scheduleSave() {
        saveTask?.cancel()
        saveTask = Task {
            try? await Task.sleep(for: .seconds(3))
            guard !Task.isCancelled else { return }
            model.saveDraft()
        }
    }

    private func attach() {
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = true
        panel.canChooseDirectories = false
        guard panel.runModal() == .OK else { return }
        for url in panel.urls {
            let type = UTType(filenameExtension: url.pathExtension)?.preferredMIMEType ?? "application/octet-stream"
            model.compose?.attachments.append(ComposeFile(name: url.lastPathComponent, mimeType: type, path: url.path))
        }
        scheduleSave()
    }
}

private struct FieldRow<Content: View>: View {
    let label: String
    @ViewBuilder let content: Content

    var body: some View {
        HStack(spacing: 12) {
            Text(label)
                .font(.system(size: 12.5))
                .foregroundStyle(Color(Theme.text3))
                .frame(width: 52, alignment: .leading)
            content
        }
        .font(.system(size: 13.5))
        .frame(maxWidth: .infinity, alignment: .leading)
        .frame(height: 40)
        .overlay(alignment: .bottom) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
    }
}

private struct SendButton: View {
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            HStack(spacing: 8) {
                Text("Send").fontWeight(.medium)
                Text("⌘↩").font(.system(size: 11)).opacity(0.75)
            }
            .font(.system(size: 12.5))
            .foregroundStyle(Color(Theme.bg))
            .padding(.horizontal, 12)
            .frame(height: 28)
            .background(RoundedRectangle(cornerRadius: 7).fill(Color(Theme.text)))
        }
        .buttonStyle(.plain)
        .padding(.leading, 6)
    }
}

extension ComposeData {
    static let blank = ComposeData(accountId: nil, to: "", cc: "", bcc: "", subject: "", body: "", inReplyTo: nil,
                                   references: [], replyingTo: nil, attachments: [], draftId: nil)

    var isEmpty: Bool {
        to.isEmpty && cc.isEmpty && bcc.isEmpty && subject.isEmpty && attachments.isEmpty
            && body.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }
}
