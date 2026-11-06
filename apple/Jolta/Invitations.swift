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
    /// The Add to calendar menu.
    case calendars(CGRect)
    case propose(CGRect)

    var anchor: CGRect {
        switch self {
        case .note(let r), .scope(let r), .calendars(let r), .propose(let r): r
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
    /// The invitation Y, M and N answer: the last one whose message is
    /// expanded, so its card is on screen.
    var shownInvitation: InvitationInfo? {
        openThread?.invitations.last { expandedMessages[$0.messageId] ?? $0.expanded }
    }

    /// The card the choices and the panel apply to: the one last used,
    /// else the one Y, M and N answer.
    var activeInvitation: InvitationInfo? {
        openThread?.invitations.first { $0.messageId == rsvpMessage } ?? shownInvitation
    }

    /// Answers the invitation on a card: the one a card's own button names,
    /// else the one on screen. Without a card on screen it does nothing.
    func answerInvitation(_ answer: InvitationAnswer, message: Int64? = nil) {
        let invitation = if let message { openThread?.invitations.first { $0.messageId == message } } else { shownInvitation }
        guard let invitation, pendingAnswer == nil, let threadID = openThread?.threadId else { return }
        let messageID = invitation.messageId
        let choices = answerChoices
        rsvpPanel = nil
        rsvpMessage = messageID
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

    /// The guest's proposal N declines when no invitation is on screen: the
    /// last one whose message is expanded.
    var shownProposal: ProposalInfo? {
        openThread?.proposals.last { expandedMessages[$0.messageId] ?? $0.expanded }
    }

    /// P: opens Propose new time on the card on screen.
    func openPropose() {
        guard let invitation = shownInvitation, invitation.newTime != nil else {
            notify("No invitation on screen takes a new time")
            return
        }
        proposeFor = invitation.messageId
        let length = invitation.newTime.map { ($0.end - $0.start) / 60 } ?? 30
        findTime(for: .propose(message: invitation.messageId, organizer: invitation.organizer), minutes: length)
    }

    /// Proposes a new time with a Maybe or a Decline for the card's
    /// invitation, the occurrence it shows for a series.
    func proposeNewTime(_ answer: InvitationAnswer, start: Date, end: Date, note: String, message: Int64) {
        guard pendingAnswer == nil, let threadID = openThread?.threadId else { return }
        rsvpPanel = nil
        rsvpMessage = message
        pendingAnswer = answer
        Task {
            do {
                let text = try await core.proposeNewTime(
                    messageId: message, answer: answer, start: Int64(start.timeIntervalSince1970), end: Int64(end.timeIntervalSince1970), note: note)
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

    /// Decline proposal on a guest's proposal. The card comes back from the
    /// store reading Declined.
    func declineProposal(message: Int64) {
        guard pendingDecline == nil, let threadID = openThread?.threadId else { return }
        pendingDecline = message
        Task {
            do {
                let text = try await core.declineProposal(messageId: message)
                if let view = try? await core.openThread(threadId: threadID, appCss: ReaderStyle.css, utcOffset: utcOffset),
                   selectedThreadID == threadID {
                    openThread = view
                }
                notify(text)
            } catch {
                notify("\(error)")
            }
            pendingDecline = nil
        }
    }

    /// Takes the meeting a cancellation's card names out of the calendar.
    /// The card comes back from the store without the link.
    func removeCancelled(message: Int64) {
        guard pendingRemoval == nil, let threadID = openThread?.threadId else { return }
        pendingRemoval = message
        Task {
            do {
                let text = try await core.removeCancelled(messageId: message)
                if let view = try? await core.openThread(threadId: threadID, appCss: ReaderStyle.css, utcOffset: utcOffset),
                   selectedThreadID == threadID {
                    openThread = view
                }
                notify(text)
            } catch {
                notify("\(error)")
            }
            pendingRemoval = nil
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
        guard let v = await call(script, arguments: ["id": String(message), "selector": selector]) as? [Double], v.count == 4 else {
            return nil
        }
        return CGRect(x: v[0], y: v[1], width: v[2], height: v[3])
    }

    /// Marks a card's Remove from calendar or Decline proposal while it
    /// goes out.
    func markSending(message: Int64, on: Bool) {
        let script = """
        const card = document.querySelector(`[data-rsvp="${id}"]`);
        if (card) card.classList.toggle("jolta-sending", on);
        """
        run(script, arguments: ["id": String(message), "on": on])
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
        run(script, arguments: args)
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

/// Propose new time: the new start and end, the end keeping the meeting's
/// length until it is changed, a note, and the answer that goes with it.
/// Tab moves through the fields, ↑ and ↓ change the part of a date under
/// the cursor, Return proposes with Maybe, ⌥Return with Decline, Esc closes.
struct RsvpProposePanel: View {
    @Bindable var model: AppModel
    let invitation: InvitationInfo
    @State private var start: Date
    @State private var end: Date
    @State private var length: TimeInterval
    @State private var note = ""
    @FocusState private var focus: Field?

    private enum Field { case start, end, note }

    init(model: AppModel, invitation: InvitationInfo, slot: TimeSlot) {
        self.model = model
        self.invitation = invitation
        let slot = model.proposedSlot ?? slot
        let start = Date(timeIntervalSince1970: TimeInterval(slot.start))
        let end = Date(timeIntervalSince1970: TimeInterval(slot.end))
        _start = State(initialValue: start)
        _end = State(initialValue: end)
        _length = State(initialValue: end.timeIntervalSince(start))
    }

    private var organizer: String { invitation.organizer.isEmpty ? "the organizer" : invitation.organizer }
    private var valid: Bool { end > start }
    private var sameDay: Bool { Calendar.current.isDate(start, inSameDayAs: end) }

    /// "Thu 5 Nov · 45 minutes", with the end's day when it differs.
    private var summary: String {
        guard valid else { return "The new time ends before it starts" }
        let f = DateComponentsFormatter()
        f.allowedUnits = [.day, .hour, .minute]
        f.unitsStyle = .full
        let span = f.string(from: end.timeIntervalSince(start)) ?? ""
        let day = { (d: Date) in d.formatted(.dateTime.weekday(.abbreviated).day().month(.abbreviated)) }
        return sameDay ? "\(day(start)) · \(span)" : "\(day(start)) to \(day(end)) · \(span)"
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 9) {
            Text("New time for \(organizer)")
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(Color(Theme.text3))
            HStack(spacing: 6) {
                DatePicker("Starts", selection: $start, displayedComponents: [.date, .hourAndMinute])
                    .focused($focus, equals: .start)
                    .accessibilityIdentifier("propose-start")
                Text("to").font(.system(size: 12)).foregroundStyle(Color(Theme.text3))
                DatePicker("Ends", selection: $end, displayedComponents: sameDay ? [.hourAndMinute] : [.date, .hourAndMinute])
                    .focused($focus, equals: .end)
                    .accessibilityIdentifier("propose-end")
            }
            .labelsHidden()
            .datePickerStyle(.field)
            .font(.system(size: 12.5))
            HStack {
                Text(summary)
                    .font(.system(size: 11))
                    .foregroundStyle(Color(valid ? Theme.text3 : Theme.accent))
                Spacer(minLength: 0)
                Button("Find a time…") {
                    model.findTime(for: .propose(message: invitation.messageId, organizer: invitation.organizer),
                                   minutes: Int64(end.timeIntervalSince(start) / 60))
                }
                .buttonStyle(.link)
                .font(.system(size: 11))
                .help("See when \(organizer) and the guests are busy")
                .accessibilityIdentifier("propose-find-time")
            }
            TextField("Note to \(organizer) (optional)", text: $note)
                .textFieldStyle(.plain)
                .font(.system(size: 13))
                .padding(.horizontal, 8)
                .frame(height: 28)
                .background(RoundedRectangle(cornerRadius: 6).fill(Color(Theme.bg)))
                .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color(Theme.line), lineWidth: 1))
                .focused($focus, equals: .note)
                .onSubmit { send(.maybe) }
                .accessibilityIdentifier("propose-note")
            HStack(spacing: 6) {
                Text("Propose with")
                    .font(.system(size: 11))
                    .foregroundStyle(Color(Theme.text3))
                Spacer(minLength: 0)
                answer(.decline, hint: "⌥↩")
                    .keyboardShortcut(.return, modifiers: .option)
                answer(.maybe, hint: "↩")
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(12)
        .frame(width: 330)
        .background(RoundedRectangle(cornerRadius: 10).fill(Color(Theme.raise)))
        .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Color(Theme.line), lineWidth: 0.5))
        .shadow(color: .black.opacity(0.25), radius: 18, y: 8)
        .onExitCommand { model.rsvpPanel = nil }
        .onAppear { if model.findTime == nil { focus = .start } }
        .onDisappear { model.proposedSlot = nil }
        .onChange(of: model.findTime) { _, new in if new == nil { focus = .start } }
        .onChange(of: model.proposedSlot) { _, new in
            guard let new else { return }
            let (s, e) = (Date(timeIntervalSince1970: TimeInterval(new.start)), Date(timeIntervalSince1970: TimeInterval(new.end)))
            length = e.timeIntervalSince(s)
            start = s
            end = e
        }
        .onChange(of: start) { _, new in end = new.addingTimeInterval(length) }
        .onChange(of: end) { _, new in
            if new > start { length = new.timeIntervalSince(start) }
        }
    }

    private func answer(_ a: InvitationAnswer, hint: String) -> some View {
        let main = a == .maybe
        return Button { send(a) } label: {
            HStack(spacing: 5) {
                Text(a.title)
                Text(hint).foregroundStyle(Color(Theme.text3))
            }
            .font(.system(size: 12, weight: main ? .semibold : .medium))
            .padding(.horizontal, 10)
            .frame(height: 24)
            .background(RoundedRectangle(cornerRadius: 6).fill(main ? Color(Theme.fill) : .clear))
            .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color(Theme.line), lineWidth: 1))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .foregroundStyle(Color(Theme.text))
        .disabled(!valid)
        .accessibilityIdentifier("propose-\(a.name)")
    }

    private func send(_ a: InvitationAnswer) {
        guard valid else { return }
        model.proposeNewTime(a, start: start, end: end, note: note.trimmingCharacters(in: .whitespacesAndNewlines), message: invitation.messageId)
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
