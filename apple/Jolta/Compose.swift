@preconcurrency import JoltaFFI
import SwiftUI
import UniformTypeIdentifiers

/// Writing a message, in place of the reader. ⌘↩ sends (after the undo
/// window), Esc closes and keeps a draft.
struct ComposeView: View {
    @Bindable var model: AppModel
    @State private var showCc = false
    @FocusState private var focus: Field?
    @State private var saveTask: Task<Void, Never>?
    /// Writing with AI (⌘J), while its bar is open.
    @State private var writer: AIWriter?

    enum Field { case to, cc, bcc, subject, body, ai }

    private var draft: Binding<ComposeData> {
        // A field that commits as the view closes must not reopen a discarded or sent message.
        Binding(get: { model.compose ?? ComposeData.blank }, set: {
            guard model.compose != nil else { return }
            model.compose = $0
            scheduleSave()
        })
    }

    /// Each account that can send, with its aliases.
    @State private var senders: [MailSender] = []
    @AppStorage(AppModel.key("showsFormatBar")) private var showsFormatBar = true

    private func actions(showsKeys: Bool) -> some View {
        HStack(spacing: 4) {
            ActionButton(icon: "paperclip", title: "Attach", key: "⇧⌘A", showsTitle: false, showsKey: showsKeys) { attach() }
                .keyboardShortcut("a", modifiers: [.command, .shift])
            ActionButton(icon: "sparkles", title: "Write with AI", key: "⌘J", showsTitle: false, showsKey: showsKeys) { writeWithAI() }
                .keyboardShortcut("j", modifiers: .command)
            ActionButton(icon: "text.badge.plus", title: "Insert snippet", key: "⌘;", showsTitle: false, showsKey: showsKeys) { model.palette = .snippets }
                .keyboardShortcut(";", modifiers: .command)
            ActionButton(icon: "bell", title: "Remind me if no reply", key: "⇧⌘H", showsTitle: false, showsKey: showsKeys) { model.palette = .remind }
                .keyboardShortcut("h", modifiers: [.command, .shift])
            ActionButton(icon: "clock", title: "Send later", key: "⇧⌘L", showsTitle: false, showsKey: showsKeys) { model.palette = .sendLater }
                .keyboardShortcut("l", modifiers: [.command, .shift])
            ActionButton(icon: "trash", title: "Discard", key: "⌘⌫", showsTitle: false, showsKey: showsKeys) { model.discardCompose() }
                .keyboardShortcut(.delete, modifiers: .command)
            KitButton(title: "Send", key: "⌘↩", kind: .primary) { model.sendCompose() }
                // ⌘Return is Find a time's Invite while its panel is up.
                .keyboardShortcut(model.findTime == nil ? KeyboardShortcut(.return, modifiers: .command) : nil)
                .padding(.leading, 6)
        }
        .fixedSize()
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 4) {
                Text(draft.wrappedValue.subject.isEmpty ? "New message" : draft.wrappedValue.subject)
                    .font(.system(size: 13, weight: .semibold))
                    .lineLimit(1)
                Spacer()
                // The subject gives way first, then the keys beside the buttons,
                // so Send stays in the pane at the window's narrowest.
                ViewThatFits(in: .horizontal) {
                    actions(showsKeys: true)
                    actions(showsKeys: false)
                }
            }
            .padding(.leading, 28)
            .padding(.trailing, 16)
            .frame(height: Theme.headerHeight)
            .overlay(alignment: .bottom) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }

            VStack(spacing: 0) {
                if senders.count > 1 {
                    FieldRow(label: "From") {
                        Picker("", selection: sender) {
                            SenderMenuItems(senders: senders)
                        }
                        .labelsHidden()
                        .accessibilityIdentifier("compose-from")
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
                FieldRow(label: "Subject") {
                    TextField("", text: draft.subject)
                        .accessibilityIdentifier("compose-subject")
                        .focused($focus, equals: .subject)
                        // The body is an AppKit text view, outside SwiftUI's focus order.
                        .onKeyPress(.tab) {
                            guard let body = RichTextView.messageBody else { return .ignored }
                            body.window?.makeFirstResponder(body)
                            return .handled
                        }
                    if !showsFormatBar {
                        Button("Aa") { showsFormatBar = true }
                            .buttonStyle(.plain)
                            .font(.system(size: 12))
                            .foregroundStyle(Color(Theme.text3))
                            .help("Show formatting  ⌥⌘T")
                            .accessibilityLabel("Show formatting")
                            .accessibilityIdentifier("format-show")
                    }
                }
                if let schedule = model.composeSchedule {
                    HStack(spacing: 8) {
                        Image(systemName: "clock").foregroundStyle(Color(Theme.text3))
                        Text("Scheduled for \(schedule.label). Esc keeps it scheduled.").foregroundStyle(Color(Theme.text2))
                        Button { model.composeSchedule = nil } label: {
                            Image(systemName: "xmark").font(.system(size: 9.5, weight: .semibold))
                        }
                        .buttonStyle(.plain)
                        .foregroundStyle(Color(Theme.text3))
                        .help("Don't send it then; keep it as a draft")
                        Spacer()
                    }
                    .font(.system(size: 12.5))
                    .frame(height: 34)
                    .overlay(alignment: .bottom) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
                }
                if let reminder = model.composeReminder {
                    HStack(spacing: 8) {
                        Image(systemName: "bell").foregroundStyle(Color(Theme.text3))
                        Text("Back in your inbox on \(reminder.label) if nobody replies").foregroundStyle(Color(Theme.text2))
                        Button { model.composeReminder = nil } label: {
                            Image(systemName: "xmark").font(.system(size: 9.5, weight: .semibold))
                        }
                        .buttonStyle(.plain)
                        .foregroundStyle(Color(Theme.text3))
                        .help("No reminder")
                        Spacer()
                    }
                    .font(.system(size: 12.5))
                    .frame(height: 34)
                    .overlay(alignment: .bottom) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
                }
                ImageSizeBar(model: model, changed: scheduleSave)
            }
            .padding(.horizontal, 28)
            .textFieldStyle(.plain)
            // Suggestions hang over the subject and the body.
            .zIndex(1)

            if let writer {
                WriteBar(writer: writer, model: model, focused: $focus) { closeWriter() }
                    .onChange(of: model.compose?.body) { _, body in
                        // Editing a finished draft keeps it.
                        if writer.state == .done, body != writer.lastBody { self.writer = nil }
                    }
            }

            if showsFormatBar {
                FormatBar()
            }

            // The cursor starts where the user types next: To until it names
            // someone, then the end of what they wrote, above the signature
            // and any quote.
            RichTextEditor(text: draft.body, core: model.core, isMessageBody: true) { body in
                guard let c = model.compose, !c.to.isEmpty else { focus = .to; return }
                body.focus(after: model.core.written(body: c.body, accountId: c.accountId), core: model.core)
            }
                .padding(.horizontal, 28)
                .padding(.top, 14)
                .environment(\.pasteTarget, PasteTarget(core: model.core, attach: attach, failed: model.notify))

            if !draft.wrappedValue.attachments.isEmpty {
                ScrollView(.horizontal) {
                    HStack(spacing: 8) {
                        ForEach(Array(draft.wrappedValue.attachments.enumerated()), id: \.offset) { i, file in
                            HStack(spacing: 8) {
                                FileIcon(core: model.core, name: file.name, mimeType: file.mimeType)
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
        }
        .onExitCommand {
            // Esc closes an open panel, then stops, undoes or closes AI
            // writing, and only then closes the message.
            if model.palette != nil { model.palette = nil; return }
            guard let writer else { return model.closeCompose() }
            switch writer.state {
            case .working: writer.stop()
            case .done: writer.undo(model: model); closeWriter()
            default: closeWriter()
            }
        }
        .onDisappear { saveTask?.cancel() }
        .task(id: "\(model.accounts.map(\.id)) \(model.sendersRevision)") { senders = (try? model.core.senders()) ?? [] }
    }

    /// ⌘J: opens the AI bar, or writes again from the same instruction.
    private func writeWithAI() {
        if let writer, writer.state != .asking {
            writer.write(model: model)
        } else if writer == nil {
            writer = AIWriter()
            focus = .ai
        }
    }

    private func closeWriter() {
        writer?.stop()
        writer = nil
        if let body = RichTextView.messageBody { body.window?.makeFirstResponder(body) }
        scheduleSave()
    }

    /// The From account or alias; another account swaps the signature in the body.
    private var sender: Binding<SenderKey?> {
        Binding(get: {
            guard let compose = model.compose, let account = compose.accountId else { return nil }
            // A draft from an alias removed since sends from the account.
            let key = SenderKey(accountId: account, alias: compose.from)
            return senders.contains { SenderKey($0) == key } ? key : SenderKey(accountId: account, alias: nil)
        }, set: { key in
            guard let key, let compose = model.compose else { return }
            if compose.accountId != key.accountId {
                model.compose?.body = (try? model.core.swapSignature(body: compose.body, fromAccount: compose.accountId, toAccount: key.accountId)) ?? compose.body
            }
            model.compose?.accountId = key.accountId
            model.compose?.from = senders.first { SenderKey($0) == key }?.alias
            scheduleSave()
        })
    }

    private func recipientField(_ text: Binding<String>, _ field: Field) -> some View {
        let d = draft.wrappedValue
        return RecipientField(text: text, core: model.core, account: d.accountId,
                              recipients: [d.to, d.cc, d.bcc].joined(separator: ", "), active: focus == field)
            .accessibilityIdentifier("compose-\(field)")
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
        attach(panel.urls)
    }

    private func attach(_ urls: [URL]) {
        for url in urls {
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

extension ComposeData {
    static let blank = ComposeData(accountId: nil, from: nil, to: "", cc: "", bcc: "", subject: "", body: "", inReplyTo: nil,
                                   references: [], replyingTo: nil, attachments: [], draftId: nil, imageSize: .actual)

    var isEmpty: Bool {
        to.isEmpty && cc.isEmpty && bcc.isEmpty && subject.isEmpty && attachments.isEmpty
            && body.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }
}

/// An attachment's format icon, drawn from the core's SVG so the reader and
/// compose show the same one.
struct FileIcon: View {
    let core: Core
    let name: String
    let mimeType: String

    var body: some View {
        let outline = Self.hex(Theme.text3)
        let svg = core.fileIconSvg(name: name, mimeType: mimeType, outline: outline)
        if let image = NSImage(data: Data(svg.utf8)) {
            Image(nsImage: image).frame(width: 20, height: 24)
        } else {
            Image(systemName: "doc").foregroundStyle(Color(Theme.text3))
        }
    }

    /// A theme color as `#rrggbb` in the app's current appearance.
    private static func hex(_ color: NSColor) -> String {
        var out = "#888888"
        NSApp.effectiveAppearance.performAsCurrentDrawingAppearance {
            guard let c = color.usingColorSpace(.sRGB) else { return }
            out = String(format: "#%02x%02x%02x", Int(c.redComponent * 255), Int(c.greenComponent * 255), Int(c.blueComponent * 255))
        }
        return out
    }
}

/// While the message shows pasted images: what it comes to, and the size its
/// images go out at, as Apple Mail offers it. Over Microsoft 365's usual
/// limit, it says so and offers the largest size under it.
private struct ImageSizeBar: View {
    @Bindable var model: AppModel
    let changed: () -> Void
    @State private var sizes: [MessageSize] = []

    /// What the built message depends on, apart from the image size.
    private struct Inputs: Equatable {
        let body: String
        let headers: [String]
        let files: [String]
        let account: Int64?
    }

    private var inputs: Inputs? {
        model.compose.map {
            Inputs(body: $0.body, headers: [$0.to, $0.cc, $0.bcc, $0.subject], files: $0.attachments.map(\.path), account: $0.accountId)
        }
    }

    var body: some View {
        VStack(spacing: 0) {
            if let compose = model.compose, let chosen = sizes.first(where: { $0.size == compose.imageSize }) {
                HStack(spacing: 8) {
                    Image(systemName: "photo").foregroundStyle(Color(Theme.text3))
                    Text("Message size \(Self.bytes(chosen.bytes))")
                        .foregroundStyle(Color(Theme.text2))
                        .accessibilityIdentifier("compose-message-size")
                    Menu {
                        ForEach(sizes, id: \.size) { option in
                            Button(Self.label(option)) { choose(option.size) }
                        }
                    } label: {
                        Text("Image size: \(Self.name(chosen.size))")
                    }
                    .menuStyle(.borderlessButton)
                    .fixedSize()
                    .accessibilityIdentifier("compose-image-size")
                    if !chosen.fits {
                        let fitting = sizes.last(where: \.fits)
                        Text(fitting == nil ? "Over Microsoft 365's usual 35 MB limit, even at Small" : "Over Microsoft 365's usual 35 MB limit")
                            .foregroundStyle(Color(Theme.accentText))
                        if let fitting {
                            KitButton(title: "Use \(Self.name(fitting.size))", size: .small) { choose(fitting.size) }
                        }
                    }
                    Spacer()
                }
                .font(.system(size: 12.5))
                .frame(height: 34)
                .overlay(alignment: .bottom) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
            }
        }
        .task(id: inputs) {
            guard let compose = model.compose else { return sizes = [] }
            try? await Task.sleep(for: .milliseconds(300))
            guard !Task.isCancelled else { return }
            let core = model.core
            let measured = await Task.detached(priority: .userInitiated) { (try? core.messageSizes(compose: compose)) ?? [] }.value
            if !Task.isCancelled { sizes = measured }
        }
    }

    private func choose(_ size: ImageSizeChoice) {
        model.compose?.imageSize = size
        changed()
    }

    private static func name(_ size: ImageSizeChoice) -> String {
        switch size {
        case .small: "Small"
        case .medium: "Medium"
        case .large: "Large"
        case .actual: "Actual Size"
        }
    }

    private static func label(_ option: MessageSize) -> String {
        let side = option.longestSide.map { " (\($0) px)" } ?? ""
        return "\(name(option.size))\(side) · \(bytes(option.bytes))" + (option.fits ? "" : " · too large")
    }

    private static func bytes(_ count: UInt64) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(count), countStyle: .file)
    }
}
