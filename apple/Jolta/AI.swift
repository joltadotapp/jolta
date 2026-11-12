@preconcurrency import JoltaFFI
import SwiftUI

/// Hands a request's progress from the core's threads to the main actor.
final class AIListener: AiListener, @unchecked Sendable {
    private let phase: @MainActor (AiPhase) -> Void
    private let text: @MainActor (String) -> Void
    private let finish: @MainActor (AiOutcome) -> Void

    init(phase: @escaping @MainActor (AiPhase) -> Void, text: @escaping @MainActor (String) -> Void,
         finish: @escaping @MainActor (AiOutcome) -> Void) {
        self.phase = phase
        self.text = text
        self.finish = finish
    }

    func onPhase(phase: AiPhase) { Task { @MainActor in self.phase(phase) } }
    func onText(text: String) { Task { @MainActor in self.text(text) } }
    func onFinish(outcome: AiOutcome) { Task { @MainActor in self.finish(outcome) } }
}

extension AiPhase {
    /// "Starting Gemma 4 E2B…", for whichever model runs.
    func line(_ label: String) -> String {
        switch self {
        case .starting: "Starting \(label)…"
        case .reading: "Reading the conversation…"
        case .writing: "Writing…"
        }
    }
}

// MARK: - Summary

/// The summary of the open conversation: a saved one shows at once, a new
/// or updated one streams in.
@MainActor
@Observable
final class ConversationSummary {
    enum State: Equatable {
        /// Nothing to show: short conversation, AI off, or not asked for.
        case hidden
        case working(AiPhase)
        case done
        case notSetUp
        case failed(String)
    }

    let threadID: Int64
    private(set) var state = State.hidden
    private(set) var text = ""
    private(set) var model = ""
    /// A saved summary shows while messages that arrived since are read.
    private(set) var outdated = false
    private var task: AiTask?
    /// Numbers each start, so a cancelled request's late callbacks are ignored.
    private var run = 0
    private let core: Core

    init(threadID: Int64, core: Core) {
        self.threadID = threadID
        self.core = core
        if let saved = try? core.cachedSummary(threadId: threadID) {
            text = saved.text
            model = saved.model
            outdated = !saved.current
            state = .done
        }
    }

    var isWorking: Bool { if case .working = state { true } else { false } }

    /// `automatic` runs only for long conversations, with summaries on, and
    /// stays out of sight until the model actually starts.
    func start(automatic: Bool, fresh: Bool = false, label: String) {
        task?.cancel()
        if automatic && state == .done && !outdated { return }
        run += 1
        let run = run
        if !automatic || !text.isEmpty { state = .working(.starting) }
        let listener = AIListener(
            phase: { [weak self] phase in
                guard let self, self.run == run, self.task != nil else { return }
                self.state = .working(phase)
            },
            text: { [weak self] text in
                // Text that arrives after the finish (callbacks hop threads) is stale.
                guard let self, self.run == run, self.task != nil else { return }
                self.state = .working(.writing)
                self.text = text
                self.model = label
                self.outdated = false
            },
            finish: { [weak self] outcome in
                guard let self, self.run == run else { return }
                self.finish(outcome, automatic: automatic)
            })
        task = core.summarize(threadId: threadID, automatic: automatic, fresh: fresh, listener: listener)
    }

    private func finish(_ outcome: AiOutcome, automatic: Bool) {
        task = nil
        switch outcome {
        case .done(let text, let model):
            self.text = text
            self.model = model
            outdated = false
            state = .done
        case .notNeeded:
            if text.isEmpty { state = .hidden }
        case .notSetUp:
            state = automatic ? .hidden : .notSetUp
        case .cancelled:
            state = text.isEmpty ? .hidden : .done
        case .failed(let message):
            // A background attempt that fails keeps an older summary on screen.
            state = automatic && !text.isEmpty ? .done : .failed(message)
        }
    }

    /// After new mail in the conversation: brings a shown summary up to date.
    func refresh(label: String) {
        guard state == .done, let saved = try? core.cachedSummary(threadId: threadID), !saved.current else { return }
        outdated = true
        start(automatic: true, label: label)
    }

    func cancel() {
        task?.cancel()
        task = nil
    }

    func dismiss() {
        cancel()
        state = .hidden
    }
}

/// The summary over the conversation, in the reader.
struct SummaryCard: View {
    let summary: ConversationSummary
    let label: String
    let openSettings: () -> Void
    @AppStorage("SummaryCollapsed") private var collapsed = false

    var body: some View {
        if summary.state != .hidden {
            VStack(alignment: .leading, spacing: 8) {
                header
                if !collapsed { content }
            }
            .padding(.horizontal, 14)
            .padding(.vertical, 11)
            .card()
            .padding(.horizontal, Theme.gutter)
            .padding(.top, 14)
            .padding(.bottom, 2)
            .transition(.opacity)
        }
    }

    private var header: some View {
        HStack(spacing: 7) {
            Image(systemName: "sparkles").font(.system(size: 11, weight: .medium))
            Text("Summary").font(.system(size: 12, weight: .semibold))
            if !summary.model.isEmpty && summary.state == .done {
                Text(summary.outdated ? "\(summary.model) · new messages since" : summary.model)
                    .font(.system(size: Theme.small))
                    .foregroundStyle(Color(Theme.text3))
                    .lineLimit(1)
            }
            Spacer(minLength: 8)
            if case .working(let phase) = summary.state, !(phase == .writing && !summary.text.isEmpty) {
                ProgressView().controlSize(.mini)
                Text(phase.line(label)).font(.system(size: Theme.small)).foregroundStyle(Color(Theme.text3)).lineLimit(1)
            }
            if summary.state == .done {
                iconButton("arrow.clockwise", help: "Summarize again  ⌘J") { summary.start(automatic: false, fresh: true, label: label) }
            }
            iconButton(collapsed ? "chevron.down" : "chevron.up", help: collapsed ? "Show summary" : "Hide summary") {
                withAnimation(Theme.quick) { collapsed.toggle() }
            }
        }
        .foregroundStyle(Color(Theme.text2))
    }

    @ViewBuilder private var content: some View {
        switch summary.state {
        case .notSetUp:
            HStack(spacing: 10) {
                Text("Summaries need a model: one on this Mac, or your OpenAI or Anthropic account.")
                    .foregroundStyle(Color(Theme.text2))
                Spacer(minLength: 8)
                KitButton(title: "Choose a Model…", size: .small, action: openSettings)
            }
            .font(.system(size: Theme.body))
        case .failed(let message):
            HStack(alignment: .firstTextBaseline, spacing: 10) {
                Text(message).foregroundStyle(Color(Theme.text2)).textSelection(.enabled)
                Spacer(minLength: 8)
                KitButton(title: "Try Again", size: .small) { summary.start(automatic: false, fresh: true, label: label) }
                if message.contains("Settings") { KitButton(title: "AI Settings…", size: .small, action: openSettings) }
            }
            .font(.system(size: Theme.body))
        default:
            if !summary.text.isEmpty {
                VStack(alignment: .leading, spacing: 5) {
                    ForEach(Array(lines.enumerated()), id: \.offset) { _, line in
                        HStack(alignment: .firstTextBaseline, spacing: 8) {
                            Text("•").foregroundStyle(Color(Theme.text3))
                            Text(line)
                        }
                    }
                }
                .font(.system(size: 13))
                .lineSpacing(2)
                .foregroundStyle(Color(summary.outdated ? Theme.text2 : Theme.text))
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
            }
        }
    }

    /// Bullets without their dash, with inline Markdown (a stray **bold**) rendered.
    private var lines: [AttributedString] {
        summary.text.split(separator: "\n").compactMap { raw in
            var line = raw.trimmingCharacters(in: .whitespaces)
            for prefix in ["- ", "* ", "• "] where line.hasPrefix(prefix) { line.removeFirst(prefix.count) }
            guard !line.isEmpty else { return nil }
            return (try? AttributedString(markdown: line)) ?? AttributedString(line)
        }
    }

    private func iconButton(_ icon: String, help: String, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Image(systemName: icon).font(.system(size: 10.5, weight: .semibold)).frame(width: 20, height: 20).contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .foregroundStyle(Color(Theme.text3))
        .help(help)
    }
}

// MARK: - Writing with AI

/// ⌘J while writing: an instruction in, the message streamed into the
/// body. Keep, try again, or undo back to what was there.
@MainActor
@Observable
final class AIWriter {
    enum State: Equatable {
        case asking
        case working(AiPhase)
        case done
        case notSetUp
        case failed(String)
    }

    var instruction = ""
    private(set) var state = State.asking
    private(set) var model = ""
    /// The instruction the shown draft was written from.
    private(set) var lastInstruction: String?
    /// The body as the writer last set it, to tell the user's own edits apart.
    private(set) var lastBody: String?
    /// The body before the first draft, which retries start from and undo restores.
    private var original: String?
    private var task: AiTask?

    var isWorking: Bool { if case .working = state { true } else { false } }

    func write(model app: AppModel) {
        guard let compose = app.compose else { return }
        task?.cancel()
        let base = original ?? compose.body
        original = base
        var request = compose
        request.body = base
        let (account, core) = (compose.accountId, app.core)
        lastInstruction = instruction
        state = .working(.starting)
        model = app.ai.label
        let put = { [weak self, weak app] (text: String) in
            guard let self, let app, app.compose != nil else { return }
            let body = core.withWritten(body: base, accountId: account, text: text)
            self.lastBody = body
            app.compose?.body = body
        }
        let listener = AIListener(
            phase: { [weak self] phase in if self?.isWorking == true { self?.state = .working(phase) } },
            text: { [weak self] text in if self?.isWorking == true { put(text) } },
            finish: { [weak self] outcome in
                guard let self else { return }
                self.task = nil
                switch outcome {
                case .done(let text, let label):
                    put(text)
                    self.model = label
                    self.state = .done
                case .notSetUp: self.state = .notSetUp
                case .cancelled: self.state = self.lastBody == nil ? .asking : .done
                case .notNeeded: self.state = .asking
                case .failed(let message): self.state = .failed(message)
                }
            })
        task = core.draftMessage(compose: request, instruction: instruction, listener: listener)
    }

    func stop() {
        task?.cancel()
        task = nil
    }

    /// Puts the body back as it was before the first draft.
    func undo(model app: AppModel) {
        stop()
        if let original { app.compose?.body = original }
    }
}

/// The bar above the body while writing with AI. Its field keeps the
/// keyboard: Return writes (or keeps a finished draft), ⌘J tries again, Esc
/// stops, undoes or closes.
struct WriteBar: View {
    let writer: AIWriter
    let model: AppModel
    var focused: FocusState<ComposeView.Field?>.Binding
    let close: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 10) {
                Image(systemName: "sparkles").font(.system(size: 12, weight: .medium)).foregroundStyle(Color(Theme.text2))
                TextField(placeholder, text: Bindable(writer).instruction)
                    .textFieldStyle(.plain)
                    .focused(focused, equals: .ai)
                    .onSubmit(submit)
                    .onExitCommand(perform: escape)
                controls.fixedSize()
            }
            .frame(minHeight: 22)
            if let status {
                HStack(spacing: 6) {
                    if writer.isWorking { ProgressView().controlSize(.mini) }
                    Text(status).lineLimit(2).textSelection(.enabled)
                    if writer.state == .notSetUp { KitButton(title: "Choose a Model…", size: .small) { model.openSettings(.ai) } }
                }
                .font(.system(size: Theme.small))
                .foregroundStyle(Color(Theme.text3))
                .padding(.leading, 22)
            }
        }
        .font(.system(size: 13))
        .padding(.horizontal, 12)
        .padding(.vertical, 9)
        .card()
        .padding(.horizontal, Theme.gutter)
        .padding(.top, 12)
        .onAppear { focused.wrappedValue = .ai }
    }

    @ViewBuilder private var controls: some View {
        HStack(spacing: 6) {
            switch writer.state {
            case .working:
                KitButton(title: "Stop", key: "esc", size: .small) { writer.stop() }
            case .done:
                KitButton(title: "Keep", key: "↩", size: .small, action: close)
                KitButton(title: "Try Again", key: "⌘J", size: .small) { writer.write(model: model) }
                KitButton(title: "Undo", key: "esc", size: .small) { writer.undo(model: model); close() }
            default:
                KitButton(title: "Write", key: "↩", size: .small) { writer.write(model: model) }
            }
        }
    }

    private var status: String? {
        switch writer.state {
        case .asking: nil
        case .working(let phase): phase.line(model.ai.label)
        case .done: "Written by \(writer.model). Edit it like your own text, or change the instruction and press Return."
        case .notSetUp: "Writing with AI needs a model."
        case .failed(let message): message
        }
    }

    private var placeholder: String {
        model.compose?.replyingTo != nil
            ? "What should the reply say? Rough notes are fine, or leave it empty."
            : "What should the email say? Rough notes are fine."
    }

    private func submit() {
        if writer.state == .done && writer.instruction == writer.lastInstruction { return close() }
        writer.write(model: model)
    }

    private func escape() {
        switch writer.state {
        case .working: writer.stop()
        case .done: writer.undo(model: model); close()
        default: close()
        }
    }
}
