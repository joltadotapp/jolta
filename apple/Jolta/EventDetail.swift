@preconcurrency import JoltaFFI
import SwiftUI

/// One occurrence's detail beside the event: times (with the event's own
/// zone when it differs), repeat, meeting with Join, phones and passcode,
/// place, people, calendar and reminders, and the description in the
/// reader's locked-down web view.
struct EventDetailCard: View {
    let model: AppModel
    let cal: CalendarModel
    let detail: EventDetail
    let event: CalEvent
    @State private var reader = ReaderPage()
    @State private var showsPeople = false

    private static let day: DateFormatter = {
        let f = DateFormatter()
        f.setLocalizedDateFormatFromTemplate("EEE d MMM")
        return f
    }()

    private var when: String {
        var parts: [String] = []
        switch detail.time {
        case .timed(let start, _, _, let text):
            parts.append(Self.day.string(from: Date(timeIntervalSince1970: TimeInterval(start))))
            parts.append(text)
        case .allDay(let first, let last):
            parts.append(first == last ? CalendarModel.spokenDay(first) : "\(CalendarModel.spokenDay(first)) to \(CalendarModel.spokenDay(last))")
        }
        if let repeatText = detail.repeat { parts.append(repeatText) }
        return parts.joined(separator: " · ")
    }

    private var live: Bool {
        guard case .timed(let start, let end, _, _) = detail.time else { return false }
        return CalendarModel.isLive(start: start, end: end, now: cal.now)
    }

    private var status: String? {
        if detail.status == .cancelled { return "Cancelled" }
        if detail.myResponse == .declined { return "You declined" }
        if detail.status == .tentative || detail.showAs == .tentative { return "Tentative" }
        if detail.showAs == .free, detail.kindLine == nil { return "Shows as free" }
        return nil
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                if detail.visibility == .private || detail.visibility == .confidential {
                    Image(systemName: "lock.fill").font(.system(size: 11)).foregroundStyle(Color(Theme.text3))
                }
                Text(detail.title.isEmpty ? "(no title)" : detail.title)
                    .font(.system(size: 14, weight: .semibold))
                    .strikethrough(detail.status == .cancelled)
                    .textSelection(.enabled)
                    .accessibilityIdentifier("event-detail-title")
                Spacer(minLength: 8)
                Button { cal.closeDetail() } label: { Image(systemName: "xmark").font(.system(size: 10, weight: .semibold)) }
                    .buttonStyle(.plain)
                    .foregroundStyle(Color(Theme.text3))
                    .help("Close (Esc)")
            }
            Text(when).font(.system(size: 12)).foregroundStyle(Color(Theme.text3)).padding(.top, 2)
            if let line = detail.kindLine {
                Text(line).font(.system(size: 12, weight: .medium)).foregroundStyle(Color(Theme.text2)).padding(.top, 2)
                    .accessibilityIdentifier("event-detail-kind")
            }
            if let status {
                Text(status).font(.system(size: 12, weight: .medium)).foregroundStyle(Color(Theme.text2)).padding(.top, 2)
            }
            if detail.notSynced {
                Label("Not synced yet", systemImage: "arrow.triangle.2.circlepath")
                    .font(.system(size: 12))
                    .foregroundStyle(Color(Theme.text3))
                    .padding(.top, 2)
                    .help("Jolta saved your change here and sends it as soon as it can.")
            }
            if let conflict = detail.conflict {
                VStack(alignment: .leading, spacing: 6) {
                    Label(conflict.message, systemImage: "exclamationmark.triangle")
                        .font(.system(size: 12))
                        .foregroundStyle(Color(Theme.text2))
                        .fixedSize(horizontal: false, vertical: true)
                    Button(conflict.action) { cal.applyMine(conflict.op) }
                        .controlSize(.small)
                        .accessibilityIdentifier("event-detail-apply-mine")
                }
                .padding(8)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(RoundedRectangle(cornerRadius: 6).fill(Color(Theme.fill)))
                .padding(.top, 6)
                .accessibilityIdentifier("event-detail-conflict")
            }
            VStack(alignment: .leading, spacing: 0) {
                if let m = detail.meeting {
                    row("Meeting", [m.provider.name, m.meetingId.map { "ID \($0)" }].compactMap { $0 }.joined(separator: " · "))
                    if let passcode = m.passcode { row("Passcode", passcode, mono: true) }
                    if !m.phones.isEmpty {
                        rowView("Phone") {
                            VStack(alignment: .leading, spacing: 2) {
                                ForEach(m.phones.prefix(3), id: \.self) { tel in
                                    Link(Self.phoneText(tel), destination: URL(string: tel) ?? URL(string: "tel:")!)
                                        .foregroundStyle(Color(Theme.text2))
                                }
                            }
                        }
                    }
                }
                if !detail.location.isEmpty, detail.meeting?.joinUrl != detail.location {
                    row("Where", detail.location)
                }
                if let organizer = detail.organizer {
                    // Someone else organizes a meeting the user can answer; otherwise it is the user's own.
                    rowView("Organizer") { personLink(name: organizer.name, address: organizer.address, isMe: false, linked: detail.canAnswer) }
                }
                if !detail.responses.isEmpty {
                    rowView("People") {
                        VStack(alignment: .leading, spacing: 4) {
                            Button { showsPeople.toggle() } label: {
                                HStack(spacing: 4) {
                                    Text(detail.responses)
                                    Image(systemName: showsPeople ? "chevron.up" : "chevron.down").font(.system(size: 8, weight: .bold))
                                }
                            }
                            .buttonStyle(.plain)
                            if showsPeople {
                                ForEach(detail.attendees, id: \.response) { group in
                                    FlowLayout(spacing: 4) {
                                        Text("\(group.response.word.capitalized):").foregroundStyle(Color(Theme.text3))
                                        ForEach(Array(group.people.enumerated()), id: \.offset) { i, p in
                                            personLink(name: p.name, address: p.address, isMe: p.isMe, comma: i < group.people.count - 1)
                                        }
                                    }
                                }
                                if !detail.rooms.isEmpty {
                                    Text("Rooms: " + detail.rooms.map { $0.name.isEmpty ? $0.address : $0.name }.joined(separator: ", "))
                                        .foregroundStyle(Color(Theme.text3))
                                }
                            }
                        }
                    }
                }
                if let mine = detail.myResponse, mine != .needsAction {
                    row("You", mine.word.capitalized)
                }
                let others = event.record.copies > 1 ? "also in \(event.record.copies - 1) other calendar\(event.record.copies > 2 ? "s" : "")" : nil
                row("Calendar", ([detail.calendarName] + [others, Self.reminders(detail.alarms)].compactMap { $0 }).joined(separator: " · "))
            }
            .padding(.top, 8)
            if detail.canAnswer {
                EventAnswerRow(model: model, detail: detail, event: event)
            }
            if !detail.descriptionHtml.isEmpty {
                ReaderPageView(page: reader)
                    .frame(height: 120)
                    .padding(.top, 6)
                    .overlay(alignment: .top) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
            }
            HStack(spacing: 6) {
                Button { cal.edit(event) } label: { HStack(spacing: 5) { Text("Edit"); KeyCap(key: "E") } }
                    .accessibilityIdentifier("event-detail-edit")
                if cal.detailDeletable {
                    Button { cal.deleteSelected() } label: { HStack(spacing: 5) { Text("Delete"); KeyCap(key: "⌫") } }
                        .accessibilityIdentifier("event-detail-delete")
                }
                if detail.emailGuests {
                    Button { cal.emailGuests(event) } label: { Text("Email guests") }
                        .help("Write to the people on this event, for what isn't a calendar update")
                        .accessibilityIdentifier("event-detail-email-guests")
                }
            }
            .buttonStyle(.plain)
            .font(.system(size: 11.5))
            .foregroundStyle(Color(Theme.text2))
            .padding(.top, 10)
            if let m = detail.meeting {
                HStack(spacing: 6) {
                    JoinButton(title: m.provider.short.isEmpty ? "Join" : "Join \(m.provider.short)", live: live) { CalendarModel.join(m) }
                    Button("Copy Link") {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(m.joinUrl, forType: .string)
                    }
                    .buttonStyle(.plain)
                    .font(.system(size: 11.5))
                    .foregroundStyle(Color(Theme.text2))
                    .padding(.horizontal, 8)
                    .frame(height: 22)
                    .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color(Theme.line), lineWidth: 1))
                }
                .padding(.top, 10)
            }
        }
        .font(.system(size: 12))
        .foregroundStyle(Color(Theme.text))
        .padding(.horizontal, 16)
        .padding(.vertical, 14)
        .frame(width: 340, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: 12).fill(Color(Theme.raise)))
        .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(Color(Theme.line), lineWidth: 0.5))
        .shadow(color: .black.opacity(0.3), radius: 28, y: 14)
        .onAppear(perform: render)
        .onChange(of: detail.descriptionHtml) { render() }
        .onChange(of: ThemeStore.shared.active) { reader.applyTheme() }
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Event detail")
    }

    /// A person's name, which opens a mail to just them; the user is "you".
    @ViewBuilder private func personLink(name: String, address: String, isMe: Bool, linked: Bool = true, comma: Bool = false) -> some View {
        let shown = isMe ? "you" : name.isEmpty ? address : name
        let text = comma ? shown + "," : shown
        if isMe || !linked || !detail.emailGuests {
            Text(text).foregroundStyle(Color(Theme.text3)).textSelection(.enabled)
        } else {
            Button { cal.emailGuests(event, address: address) } label: { Text(text).underline(color: Color(Theme.text3).opacity(0.5)) }
                .buttonStyle(.plain)
                .foregroundStyle(Color(Theme.text2))
                .help("Write to \(address)")
                .accessibilityIdentifier("event-detail-person")
        }
    }

    private func render() {
        guard !detail.descriptionHtml.isEmpty else { return }
        reader.show(html: detail.descriptionHtml)
    }

    private func row(_ label: String, _ value: String, mono: Bool = false) -> some View {
        rowView(label) {
            Text(value)
                .font(mono ? .system(size: 12, design: .monospaced) : .system(size: 12))
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private func rowView(_ label: String, @ViewBuilder _ value: () -> some View) -> some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Text(label).fontWeight(.medium).foregroundStyle(Color(Theme.text3)).frame(width: 66, alignment: .leading)
            value().foregroundStyle(Color(Theme.text2)).frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.vertical, 5)
        .overlay(alignment: .top) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
    }

    /// `tel:+3225886312,,349112803#` read as `+32 2 588 63 12`.
    static func phoneText(_ tel: String) -> String {
        let number = tel.dropFirst("tel:".count).split(separator: ",").first.map(String.init) ?? tel
        return number.removingPercentEncoding ?? number
    }

    /// "reminder 5 min before", "reminders 10 and 5 min before", or nil.
    static func reminders(_ alarms: [AlarmInfo]) -> String? {
        let minutes = alarms.compactMap { a -> Int64? in
            if case .before(let m, let email) = a, !email { return m }
            return nil
        }
        guard !minutes.isEmpty else { return alarms.isEmpty ? nil : "reminder set" }
        let words = minutes.sorted(by: >).map { m -> String in
            if m == 0 { return "at the start" }
            if m < 0 { return "\(-m) min after" }
            if m % 1440 == 0 { return "\(m / 1440) day\(m == 1440 ? "" : "s") before" }
            if m % 60 == 0 { return "\(m / 60) h before" }
            return "\(m) min before"
        }
        return (minutes.count == 1 ? "reminder " : "reminders ") + ListFormatter.localizedString(byJoining: words)
    }
}
