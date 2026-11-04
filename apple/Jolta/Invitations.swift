@preconcurrency import JoltaFFI
import SwiftUI

/// How the next answer to an invitation goes out. The core decides who
/// tells the organizer; these only say what the user chose.
struct AnswerChoices: Equatable {
    var note = ""
    /// "Don't send a reply".
    var quiet = false
    /// "This occurrence only", for a series.
    var occurrenceOnly = false

    var options: AnswerOptions { AnswerOptions(note: note, notify: !quiet, occurrenceOnly: occurrenceOnly) }
}

/// The small panel under a card's option link, placed at the link's frame
/// in the reader's web view.
enum RsvpPanel: Equatable {
    case note(CGRect)
    case scope(CGRect)

    var anchor: CGRect {
        switch self {
        case .note(let r), .scope(let r): r
        }
    }
}

extension InvitationAnswer {
    static let all: [InvitationAnswer] = [.accept, .maybe, .decline]

    /// As the card's links and data attributes name it.
    var name: String {
        switch self {
        case .accept: "accept"
        case .maybe: "maybe"
        case .decline: "decline"
        }
    }

    init?(name: String) {
        guard let match = Self.all.first(where: { $0.name == name }) else { return nil }
        self = match
    }

    var title: String {
        switch self {
        case .accept: "Accept"
        case .maybe: "Maybe"
        case .decline: "Decline"
        }
    }

    var done: String {
        switch self {
        case .accept: "Accepted"
        case .maybe: "Maybe"
        case .decline: "Declined"
        }
    }
}

extension EventResponse {
    var invitationAnswer: InvitationAnswer? {
        switch self {
        case .accepted: .accept
        case .tentative: .maybe
        case .declined: .decline
        case .needsAction, .delegated: nil
        }
    }
}

extension AppModel {
    /// Answers the invitation on a card, the newest one in the open
    /// conversation unless a card's own button names its message.
    func answerInvitation(_ answer: InvitationAnswer, message: Int64? = nil) {
        guard let invitation = openThread?.invitation else { return notify("No invitation to answer in this conversation") }
        guard pendingAnswer == nil, let threadID = openThread?.threadId else { return }
        let messageID = message ?? invitation.messageId
        let choices = answerChoices
        rsvpPanel = nil
        pendingAnswer = answer
        Task {
            do {
                let text = try await core.answerInvitation(messageId: messageID, answer: answer, options: choices.options)
                // The card comes back from the store with the answer before
                // the pending mark goes, so it never shows the old one.
                if let view = try? await core.openThread(threadId: threadID, appCss: ReaderStyle.css, utcOffset: utcOffset),
                   selectedThreadID == threadID {
                    openThread = view
                }
                answerChoices = AnswerChoices()
                pendingAnswer = nil
                notify(text)
            } catch {
                pendingAnswer = nil
                notify("\(error)")
            }
        }
    }

    /// Answers an event from the calendar's detail. The detail redraws when
    /// the core reports the calendar changed.
    func answerEvent(_ eventID: Int64, time: OccurrenceTime?, answer: InvitationAnswer, choices: AnswerChoices) async -> Bool {
        do {
            notify(try await core.answerEvent(eventId: eventID, time: time, answer: answer, options: choices.options))
            return true
        } catch {
            notify("\(error)")
            return false
        }
    }
}

extension ReaderPage {
    /// The frame of a card's link in the web view's coordinates.
    func frame(of selector: String, message: Int64) async -> CGRect? {
        let script = """
        const el = document.querySelector(`[data-rsvp="${id}"] ${selector}`);
        if (!el) return null;
        const r = el.getBoundingClientRect();
        return [r.left, r.top, r.width, r.height];
        """
        guard let v = try? await page.callJavaScript(script, arguments: ["id": String(message), "selector": selector]) as? [Double], v.count == 4 else {
            return nil
        }
        return CGRect(x: v[0], y: v[1], width: v[2], height: v[3])
    }

    /// Shows the choices and the answer going out on the card, with the
    /// app's own script: the page runs none of its own.
    func reflect(message: Int64, chosen: InvitationAnswer?, sending: Bool, choices: AnswerChoices, day: String?) {
        let script = """
        const card = document.querySelector(`[data-rsvp="${id}"]`);
        if (!card) return;
        card.classList.toggle("jolta-sending", sending);
        for (const b of card.querySelectorAll(".jolta-rsvp a")) {
          const on = b.dataset.answer === chosen;
          b.classList.toggle("jolta-chosen", on);
          b.setAttribute("aria-pressed", on);
          b.querySelector("span").textContent = on ? b.dataset.done : b.dataset.label;
        }
        const quietLink = card.querySelector(".jolta-rsvp-quiet");
        if (quietLink) quietLink.classList.toggle("jolta-on", quiet);
        const noteLink = card.querySelector(".jolta-rsvp-note");
        if (noteLink) {
          noteLink.textContent = note ? "Note: " + note : "Add a note";
          noteLink.classList.toggle("jolta-on", !!note);
        }
        const scope = card.querySelector(".jolta-rsvp-scope");
        if (scope) {
          scope.querySelector("span").textContent = only ? "Only " + day : "Whole series";
          scope.classList.toggle("jolta-on", only);
        }
        """
        let args: [String: Any] = [
            "id": String(message), "chosen": chosen?.name ?? "", "sending": sending, "quiet": choices.quiet,
            "note": choices.note.trimmingCharacters(in: .whitespacesAndNewlines), "only": choices.occurrenceOnly, "day": day ?? "",
        ]
        Task { _ = try? await page.callJavaScript(script, arguments: args) }
    }
}

/// "Add a note": a field under the link, and the three answers to send it
/// with. Return sends it with the answer already given, else Accept.
struct RsvpNotePanel: View {
    @Bindable var model: AppModel
    let invitation: InvitationInfo
    @FocusState private var focused: Bool

    private var standard: InvitationAnswer { invitation.answer?.invitationAnswer ?? .accept }
    private var organizer: String { invitation.organizer.isEmpty ? "the organizer" : invitation.organizer }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Note to \(organizer)")
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(Color(Theme.text3))
            TextField("Running ten minutes late", text: $model.answerChoices.note)
                .textFieldStyle(.plain)
                .font(.system(size: 13))
                .padding(.horizontal, 8)
                .frame(height: 28)
                .background(RoundedRectangle(cornerRadius: 6).fill(Color(Theme.bg)))
                .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color(Theme.line), lineWidth: 1))
                .focused($focused)
                .onSubmit { model.answerInvitation(standard, message: invitation.messageId) }
                .onExitCommand { model.rsvpPanel = nil }
                .accessibilityIdentifier("rsvp-note")
            if model.answerChoices.quiet {
                Text("Don't send a reply is on, so \(organizer) won't see this note.")
                    .font(.system(size: 11))
                    .foregroundStyle(Color(Theme.text3))
                    .fixedSize(horizontal: false, vertical: true)
            }
            HStack(spacing: 6) {
                Spacer(minLength: 0)
                ForEach(InvitationAnswer.all, id: \.self) { a in
                    Button { model.answerInvitation(a, message: invitation.messageId) } label: {
                        HStack(spacing: 5) {
                            Text(a.title)
                            if a == standard { Text("↩").foregroundStyle(Color(Theme.text3)) }
                        }
                        .font(.system(size: 12, weight: a == standard ? .semibold : .medium))
                        .padding(.horizontal, 10)
                        .frame(height: 24)
                        .background(RoundedRectangle(cornerRadius: 6).fill(a == standard ? Color(Theme.fill) : .clear))
                        .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color(Theme.line), lineWidth: 1))
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .foregroundStyle(Color(Theme.text))
                }
            }
        }
        .padding(12)
        .frame(width: 300)
        .background(RoundedRectangle(cornerRadius: 10).fill(Color(Theme.raise)))
        .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Color(Theme.line), lineWidth: 0.5))
        .shadow(color: .black.opacity(0.25), radius: 18, y: 8)
        .onAppear { focused = true }
    }
}

/// The series' scope: the whole series, or the occurrence the card shows.
/// ↑ and ↓ choose, Return or Esc closes (keys in `MessageList`).
struct RsvpScopePanel: View {
    @Bindable var model: AppModel
    let day: String

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            row("Whole series", only: false)
            row("Only \(day)", only: true)
        }
        .padding(4)
        .frame(width: 190)
        .background(RoundedRectangle(cornerRadius: 9).fill(Color(Theme.raise)))
        .overlay(RoundedRectangle(cornerRadius: 9).strokeBorder(Color(Theme.line), lineWidth: 0.5))
        .shadow(color: .black.opacity(0.25), radius: 18, y: 8)
    }

    private func row(_ title: String, only: Bool) -> some View {
        let on = model.answerChoices.occurrenceOnly == only
        return Button {
            model.answerChoices.occurrenceOnly = only
            model.rsvpPanel = nil
        } label: {
            HStack(spacing: 8) {
                Image(systemName: "checkmark").font(.system(size: 10, weight: .bold)).opacity(on ? 1 : 0)
                Text(title).font(.system(size: 12.5))
                Spacer(minLength: 0)
            }
            .padding(.horizontal, 8)
            .frame(height: 26)
            .background(RoundedRectangle(cornerRadius: 6).fill(on ? Color(Theme.fill) : .clear))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .foregroundStyle(Color(Theme.text))
    }
}

/// Accept, Maybe and Decline for an event in the calendar's detail, with
/// the same choices as the invitation card, through the same core call.
struct EventAnswerRow: View {
    let model: AppModel
    let detail: EventDetail
    let event: CalEvent
    @State private var choices = AnswerChoices()
    @State private var sending: InvitationAnswer?
    @State private var showsNote = false

    private var given: InvitationAnswer? { sending ?? detail.myResponse?.invitationAnswer }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 6) {
                ForEach(InvitationAnswer.all, id: \.self) { a in
                    let on = given == a
                    Button { send(a) } label: {
                        HStack(spacing: 4) {
                            if on { Image(systemName: "checkmark").font(.system(size: 9, weight: .bold)) }
                            Text(on ? a.done : a.title)
                        }
                        .font(.system(size: 11.5, weight: on ? .semibold : .medium))
                        .padding(.horizontal, 9)
                        .frame(height: 22)
                        .background(RoundedRectangle(cornerRadius: 6).fill(on ? Color(Theme.fill) : .clear))
                        .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color(on ? Theme.text3 : Theme.line), lineWidth: 1))
                        .opacity(sending == a ? 0.6 : 1)
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .disabled(sending != nil)
                    .accessibilityIdentifier("event-\(a.name)")
                }
            }
            HStack(spacing: 14) {
                option(choices.note.isEmpty ? "Add a note" : "Note: \(choices.note)", on: !choices.note.isEmpty) { showsNote.toggle() }
                option("Don't send a reply", on: choices.quiet, box: true) { choices.quiet.toggle() }
                if detail.repeat != nil {
                    option(choices.occurrenceOnly ? "This occurrence only" : "Whole series", on: choices.occurrenceOnly) {
                        choices.occurrenceOnly.toggle()
                    }
                }
            }
            if showsNote {
                TextField("Note to \(detail.organizer.map { $0.name.isEmpty ? $0.address : $0.name } ?? "the organizer")", text: $choices.note)
                    .textFieldStyle(.plain)
                    .font(.system(size: 12))
                    .padding(.horizontal, 7)
                    .frame(height: 24)
                    .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color(Theme.line), lineWidth: 1))
                    .onSubmit { send(given ?? .accept) }
            }
        }
        .padding(.top, 10)
    }

    private func option(_ title: String, on: Bool, box: Bool = false, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            HStack(spacing: 4) {
                if box { Image(systemName: on ? "checkmark.square" : "square").font(.system(size: 10)) }
                Text(title).lineLimit(1).truncationMode(.tail)
            }
            .font(.system(size: 11))
            .foregroundStyle(Color(on ? Theme.text : Theme.text3))
        }
        .buttonStyle(.plain)
    }

    private func send(_ answer: InvitationAnswer) {
        guard sending == nil else { return }
        sending = answer
        Task {
            if await model.answerEvent(event.record.eventId, time: event.record.time, answer: answer, choices: choices) {
                choices = AnswerChoices()
                showsNote = false
            }
            sending = nil
        }
    }
}
