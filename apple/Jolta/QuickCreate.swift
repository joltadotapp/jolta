@preconcurrency import JoltaFFI
import SwiftUI

/// Q and the palette's New event: a line read into an event as it is typed,
/// the words that became fields marked, the event it would make under it.
/// Return opens the editor with that draft; nothing is saved here. G uses
/// the same panel to show a typed day.
struct QuickCreateView: View {
    @Bindable var model: AppModel
    let mode: PaletteMode
    @State private var text = ""
    @State private var preview: QuickPreview?
    @State private var day: Date?
    /// Waiting for the AI after Return, at most 5 seconds.
    @State private var reading = false
    @FocusState private var focused: Bool

    /// The app's language first, then the Mac's: a tie between readings goes to the first.
    static var languages: [String] { ["en"] + Locale.preferredLanguages }

    private var creating: Bool { mode == .quickEvent }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 12) {
                Image(systemName: creating ? "calendar.badge.plus" : "calendar")
                    .font(.system(size: 15))
                    .foregroundStyle(Color(Theme.text3))
                TextField(creating ? "Lunch with Jan fri 12:30 at Mok" : "Go to… fri, 14 dec, morgen, next week", text: $text)
                    .textFieldStyle(.plain)
                    .font(.system(size: 16))
                    .focused($focused)
                    .disabled(reading)
                    .onSubmit(submit)
                    .onChange(of: text) { refresh() }
                    .accessibilityIdentifier(creating ? "quick-create-field" : "go-to-date-field")
                if reading { ProgressView().controlSize(.small) }
            }
            .padding(.horizontal, 18)
            .frame(height: 52)
            .overlay(alignment: .bottom) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }

            VStack(alignment: .leading, spacing: 9) {
                if creating { eventLines } else { dayLine }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(.horizontal, 18)
            .padding(.vertical, 12)

            HStack(spacing: 16) {
                HStack(spacing: 5) { KeyCap(key: "↩"); Text(creating ? "open in the editor" : "go") }
                HStack(spacing: 5) { KeyCap(key: "esc"); Text("close") }
                Spacer()
                if creating { Text("English, Nederlands, Français") }
            }
            .font(.system(size: 11.5))
            .foregroundStyle(Color(Theme.text3))
            .padding(.horizontal, 18)
            .frame(height: 36)
            .overlay(alignment: .top) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
        }
        .frame(width: 560)
        .background(RoundedRectangle(cornerRadius: 14).fill(Color(Theme.raise)))
        .clipShape(RoundedRectangle(cornerRadius: 14))
        .shadow(color: .black.opacity(0.45), radius: 32, y: 16)
        .onAppear {
            focused = true
            DispatchQueue.main.async { focused = true }
        }
        .onExitCommand { close() }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier(creating ? "quick-create" : "go-to-date")
    }

    // MARK: Quick create

    @ViewBuilder private var eventLines: some View {
        if let p = preview, !text.trimmingCharacters(in: .whitespaces).isEmpty {
            Text(Self.marked(text, p.parts))
                .font(.system(size: 13))
                .lineLimit(2)
                .accessibilityIdentifier("quick-create-marked")
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Image(systemName: "calendar").foregroundStyle(Color(Theme.text3)).frame(width: 16)
                VStack(alignment: .leading, spacing: 4) {
                    Text(p.title.isEmpty ? "(no title)" : p.title).font(.system(size: 13, weight: .semibold)).lineLimit(1)
                    detail("clock", p.hasTime ? p.when : "\(p.when) · no day or time yet", tint: Self.tint(.when), muted: !p.hasTime)
                    if let r = p.repeat { detail("repeat", r, tint: Self.tint(.repeat)) }
                    if let place = p.location { detail("mappin.and.ellipse", place, tint: Self.tint(.place)) }
                    if !p.people.isEmpty { people(p.people) }
                }
            }
            .accessibilityElement(children: .combine)
            .accessibilityIdentifier("quick-create-preview")
            if p.asksAi {
                Label("The AI reads the rest when you press Return.", systemImage: "sparkles")
                    .font(.system(size: 11.5))
                    .foregroundStyle(Color(Theme.text3))
            }
        } else {
            Text("Type what, when, with whom and where, in English, Dutch or French: “Standup morgen 9u30”, “Déjeuner avec Jan vendredi 12h30”.")
                .font(.system(size: 12))
                .foregroundStyle(Color(Theme.text3))
                .fixedSize(horizontal: false, vertical: true)
        }
    }

    private func detail(_ icon: String, _ text: String, tint: Color, muted: Bool = false) -> some View {
        HStack(spacing: 6) {
            Image(systemName: icon).font(.system(size: 10.5)).foregroundStyle(tint).frame(width: 13)
            Text(text).foregroundStyle(Color(muted ? Theme.text3 : Theme.text2)).lineLimit(1)
        }
        .font(.system(size: 12))
    }

    /// A person found among those the user wrote to shows their name; one
    /// not found shows as typed with a question mark, to pick in the editor.
    private func people(_ list: [QuickPerson]) -> some View {
        HStack(spacing: 6) {
            Image(systemName: "person.2").font(.system(size: 10.5)).foregroundStyle(Self.tint(.person)).frame(width: 13)
            ForEach(Array(list.enumerated()), id: \.offset) { _, p in
                if let found = p.person {
                    Text(found.name.isEmpty ? found.address : found.name).foregroundStyle(Color(Theme.text2))
                } else {
                    Text("\(p.typed)?")
                        .foregroundStyle(Color(NSColor(hex: 0xC9302C)))
                        .help("You'll choose who \(p.typed) is in the editor; nobody is invited by a guess.")
                }
            }
        }
        .font(.system(size: 12))
        .lineLimit(1)
    }

    static func tint(_ kind: QuickPartKind) -> Color {
        switch kind {
        case .when: Color(Theme.accent)
        case .person: Color(NSColor(hex: 0x2E9E6A))
        case .place: Color(NSColor(hex: 0x3B82C4))
        case .repeat: Color(NSColor(hex: 0x8E5CD9))
        }
    }

    /// The typed line with each part that became a field tinted.
    static func marked(_ text: String, _ parts: [QuickPart]) -> AttributedString {
        let ns = text as NSString
        var out = AttributedString()
        var at = 0
        func plain(_ range: NSRange) {
            var s = AttributedString(ns.substring(with: range))
            s.foregroundColor = Color(Theme.text2)
            out += s
        }
        for p in parts where Int(p.start) >= at && Int(p.end) <= ns.length {
            if Int(p.start) > at { plain(NSRange(location: at, length: Int(p.start) - at)) }
            var s = AttributedString(ns.substring(with: NSRange(location: Int(p.start), length: Int(p.end - p.start))))
            s.foregroundColor = tint(p.kind)
            s.backgroundColor = tint(p.kind).opacity(0.14)
            out += s
            at = Int(p.end)
        }
        if at < ns.length { plain(NSRange(location: at, length: ns.length - at)) }
        return out
    }

    // MARK: Go to date

    @ViewBuilder private var dayLine: some View {
        if let day {
            Label(Self.dayFormat.string(from: day), systemImage: "arrow.right")
                .font(.system(size: 13))
                .foregroundStyle(Color(Theme.text))
                .accessibilityIdentifier("go-to-date-day")
        } else {
            Text(text.isEmpty ? "A day in English, Dutch or French: “fri”, “next tue”, “14 dec”, “overmorgen”, “2027-01-04”." : "That isn't a day yet.")
                .font(.system(size: 12))
                .foregroundStyle(Color(Theme.text3))
        }
    }

    private static let dayFormat: DateFormatter = {
        let f = DateFormatter()
        f.setLocalizedDateFormatFromTemplate("EEEE d MMMM yyyy")
        return f
    }()

    private static let keyFormat: DateFormatter = {
        let f = DateFormatter()
        f.dateFormat = "yyyy-MM-dd"
        f.locale = Locale(identifier: "en_US_POSIX")
        return f
    }()

    // MARK: Actions

    private func refresh() {
        if creating {
            preview = try? model.core.quickParse(text: text, languages: Self.languages)
        } else {
            day = ((try? model.core.goToDate(text: text, languages: Self.languages)) ?? nil).flatMap { Self.keyFormat.date(from: $0) }
        }
    }

    private func submit() {
        guard !reading else { return }
        if !creating {
            guard let day else { return }
            close()
            model.mode = .calendar
            model.calendar.go(to: day)
            return
        }
        guard !text.trimmingCharacters(in: .whitespaces).isEmpty else { return }
        reading = true
        let line = text
        Task {
            do {
                let q = try await model.core.quickDraft(text: line, languages: Self.languages)
                reading = false
                model.palette = nil
                model.mode = .calendar
                model.calendar.openQuick(q)
            } catch {
                reading = false
                model.notify("\(error)")
            }
        }
    }

    private func close() {
        model.palette = nil
        if model.mode == .mail { model.requestListFocus() }
    }
}
