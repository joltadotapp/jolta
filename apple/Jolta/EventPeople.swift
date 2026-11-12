@preconcurrency import JoltaFFI
import SwiftUI

/// The editor's People row for a meeting the user organizes: a chip per
/// person, typed names suggested as in compose. ⇥ on a selected chip
/// switches it between Required and Optional, ⌫ removes it, ← and → move
/// along the chips, and ⌫ in the empty field selects the last one.
struct PeopleField: View {
    let editor: EventEditorModel
    let account: Int64?
    @FocusState.Binding var focus: EventEditorPanel.Focus?
    @State private var text = ""
    @State private var suggestions: [PersonInfo] = []
    @State private var highlighted = 0
    /// The chip ⇥ and ⌫ act on, by address.
    @State private var selected: String?

    /// Everyone but the user, whom the chips leave out.
    private var guests: [DraftPerson] { editor.draft.people.filter { !$0.isMe } }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            FlowLayout(spacing: 5) {
                ForEach(editor.draft.unresolved, id: \.typed) { name in
                    UnresolvedChip(name: name) { editor.resolve(name, as: $0) } remove: {
                        editor.draft.unresolved.removeAll { $0.typed == name.typed }
                    }
                }
                ForEach(guests, id: \.address) { p in
                    PersonChip(person: p, saved: !editor.isNew, selected: selected == p.address) {
                        selected = p.address
                        focus = .people
                    }
                }
                input
            }
            .padding(.horizontal, 5)
            .padding(.vertical, 4)
            .frame(maxWidth: .infinity, minHeight: 26, alignment: .leading)
            .background(RoundedRectangle(cornerRadius: 6).fill(Color(Theme.raise)))
            .overlay(RoundedRectangle(cornerRadius: 6).strokeBorder(Color(focus == .people ? Theme.accent : Theme.line), lineWidth: 1))
            .overlay(alignment: .topLeading) {
                if focus == .people && !suggestions.isEmpty { list.offset(y: 30) }
            }
            .zIndex(1)
            if !guests.isEmpty {
                Text(selected == nil ? "⇥ on a person switches Required and Optional" : "⇥ switches Required and Optional, ⌫ removes")
                    .font(.system(size: 11.5))
                    .foregroundStyle(Color(Theme.text3))
            }
        }
    }

    private var input: some View {
        TextField(guests.isEmpty ? "Add people" : "", text: $text)
            .textFieldStyle(.plain)
            .frame(minWidth: 90)
            .frame(height: 20)
            .focused($focus, equals: .people)
            .onChange(of: text) {
                selected = nil
                refresh()
            }
            .onChange(of: focus) { if focus != .people { selected = nil; suggestions = [] } }
            .onKeyPress(keys: [.tab, .delete, .leftArrow, .rightArrow, .upArrow, .downArrow, .return, .escape], phases: .down) { press in
                key(press)
            }
            .onSubmit { addTyped() }
            .accessibilityIdentifier("editor-people")
    }

    private var list: some View {
        VStack(spacing: 0) {
            ForEach(Array(suggestions.enumerated()), id: \.offset) { i, person in
                SuggestionRow(person: person, on: i == highlighted) {
                    add(person)
                } forget: {
                    try? editor.core.forgetAddress(address: person.address)
                    refresh()
                }
            }
        }
        .padding(5)
        .frame(maxWidth: .infinity)
        .menuPanel()
    }

    private func key(_ press: KeyPress) -> KeyPress.Result {
        if !suggestions.isEmpty {
            switch press.key {
            case .downArrow: highlighted = (highlighted + 1) % suggestions.count; return .handled
            case .upArrow: highlighted = (highlighted - 1 + suggestions.count) % suggestions.count; return .handled
            case .return, .tab: add(suggestions[highlighted]); return .handled
            case .escape: suggestions = []; return .handled
            default: break
            }
        }
        if let chosen = selected {
            switch press.key {
            case .tab: toggleRole(chosen); return .handled
            case .delete: remove(chosen); return .handled
            case .leftArrow: step(-1); return .handled
            case .rightArrow: step(1); return .handled
            case .escape: selected = nil; return .handled
            default: return .ignored
            }
        }
        if press.key == .delete, text.isEmpty, let last = guests.last {
            selected = last.address
            return .handled
        }
        if press.key == .leftArrow, text.isEmpty, let last = guests.last {
            selected = last.address
            return .handled
        }
        if press.key == .tab, !text.trimmingCharacters(in: .whitespaces).isEmpty {
            addTyped()
            return .handled
        }
        return .ignored
    }

    private func refresh() {
        let word = text.trimmingCharacters(in: .whitespaces)
        guard !word.isEmpty else { return suggestions = [] }
        let taken = editor.draft.people.map(\.address).joined(separator: ", ")
        suggestions = (try? editor.core.suggestAddresses(typed: word, accountId: account, recipients: taken)) ?? []
        highlighted = 0
    }

    /// A typed address, or several separated by commas, as they are.
    private func addTyped() {
        let parts = text.split(whereSeparator: { $0 == "," || $0 == ";" }).map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
        guard !parts.isEmpty else { return }
        for part in parts { add(PersonInfo(name: "", address: part)) }
    }

    private func add(_ person: PersonInfo) {
        text = ""
        suggestions = []
        guard !editor.draft.people.contains(where: { $0.address.caseInsensitiveCompare(person.address) == .orderedSame }) else { return }
        editor.draft.people.append(DraftPerson(name: person.name, address: person.address, role: .required, response: .needsAction, comment: "", isMe: false))
    }

    private func toggleRole(_ address: String) {
        guard let i = editor.draft.people.firstIndex(where: { $0.address == address }) else { return }
        editor.draft.people[i].role = editor.draft.people[i].role == .optional ? .required : .optional
    }

    private func remove(_ address: String) {
        let order = guests.map(\.address)
        editor.draft.people.removeAll { $0.address == address }
        let left = guests.map(\.address)
        if let i = order.firstIndex(of: address), !left.isEmpty { selected = left[min(i, left.count - 1)] } else { selected = nil }
    }

    private func step(_ by: Int) {
        let order = guests.map(\.address)
        guard let s = selected, let i = order.firstIndex(of: s) else { return }
        let j = i + by
        selected = order.indices.contains(j) ? order[j] : nil
    }
}

/// A name quick create couldn't tie to one person the user wrote to. Its
/// menu lists who it could be and Remove; saving waits until one is chosen.
private struct UnresolvedChip: View {
    let name: UnresolvedPerson
    let pick: (PersonRecord) -> Void
    let remove: () -> Void

    private static var warning: Color { Color(Theme.dangerText) }

    var body: some View {
        Menu {
            ForEach(name.choices, id: \.address) { p in
                Button(p.name.isEmpty ? p.address : "\(p.name) · \(p.address)") { pick(p) }
            }
            if !name.choices.isEmpty { Divider() }
            Button("Remove \(name.typed)", action: remove)
        } label: {
            HStack(spacing: 4) {
                Image(systemName: "questionmark.circle").font(.system(size: 10.5, weight: .semibold))
                Text(name.typed).lineLimit(1)
            }
            .font(.system(size: 12))
            .foregroundStyle(Self.warning)
            .padding(.horizontal, 7)
            .frame(height: 21)
            .overlay(Capsule().strokeBorder(Self.warning.opacity(0.7), style: StrokeStyle(lineWidth: 1, dash: [3, 2])))
            .contentShape(Capsule())
        }
        .menuStyle(.button)
        .buttonStyle(.plain)
        .menuIndicator(.hidden)
        .fixedSize()
        .help(name.choices.isEmpty ? "Nobody you wrote to is called \(name.typed). Type their address, or remove the name." : "Choose who \(name.typed) is")
        .accessibilityLabel("\(name.typed), not chosen yet")
        .accessibilityIdentifier("unresolved-chip")
    }
}

/// One person: initials, the name, "Optional", and once the meeting is
/// saved their answer, with their comment on hover.
private struct PersonChip: View {
    let person: DraftPerson
    let saved: Bool
    let selected: Bool
    let select: () -> Void

    private var name: String { person.name.isEmpty ? person.address : person.name }

    private var initials: String {
        let words = name.split(whereSeparator: { $0 == " " || $0 == "." || $0 == "@" }).prefix(2)
        return words.compactMap(\.first).map { String($0).uppercased() }.joined()
    }

    var body: some View {
        HStack(spacing: 5) {
            Text(initials)
                .font(.system(size: 8.5, weight: .semibold))
                .foregroundStyle(Color(Theme.text2))
                .frame(width: 17, height: 17)
                .background(Circle().fill(Color(Theme.fill)))
            Text(name).lineLimit(1)
            if person.role == .optional {
                Text("Optional").font(.system(size: 10.5)).foregroundStyle(Color(selected ? Theme.onAccent : Theme.text3)).opacity(selected ? 0.8 : 1)
            }
            if saved, let glyph = answerGlyph {
                Image(systemName: glyph.0).font(.system(size: 9.5, weight: .semibold)).foregroundStyle(selected ? Color(Theme.onAccent) : glyph.1)
            }
        }
        .font(.system(size: 12))
        .foregroundStyle(Color(selected ? Theme.onAccent : Theme.text))
        .padding(.leading, 3)
        .padding(.trailing, 7)
        .frame(height: 21)
        .background(Capsule().fill(Color(selected ? Theme.accent : Theme.kbd)))
        .overlay(Capsule().strokeBorder(Color(Theme.line), lineWidth: selected ? 0 : 1))
        .contentShape(Capsule())
        .onTapGesture(perform: select)
        .help(help)
        .accessibilityElement(children: .combine)
        .accessibilityLabel("\(name), \(person.role == .optional ? "optional" : "required"), \(person.response.word)")
        .accessibilityAddTraits(selected ? .isSelected : [])
        .accessibilityIdentifier("person-chip")
    }

    private var answerGlyph: (String, Color)? {
        switch person.response {
        case .accepted: ("checkmark", Color(Theme.text2))
        case .tentative: ("questionmark", Color(Theme.text2))
        case .declined: ("xmark", Color(Theme.dangerText))
        case .needsAction, .delegated: nil
        }
    }

    private var help: String {
        let answer = person.response == .needsAction ? "No answer yet" : person.response.word.capitalized
        return person.comment.isEmpty ? "\(person.address) · \(answer)" : "\(person.address) · \(answer): \"\(person.comment)\""
    }
}

/// Request responses, Allow new time proposals, Hide the guest list, and
/// on Google Guests can invite others and Guests can edit. An option the
/// calendar lacks stays in place, greyed, so the panel keeps its shape.
struct OrganizerOptions: View {
    let editor: EventEditorModel
    let caps: CapabilitiesInfo?
    let google: Bool

    var body: some View {
        VStack(spacing: 2) {
            row("Request responses", has: caps?.requestResponses == true, tag: "Not on Google", \.requestResponses)
            row("Allow new time proposals", has: caps?.allowNewTimeProposals == true, tag: "Not on Google", \.allowNewTimes)
            row("Hide the guest list", has: caps?.hideGuestList == true, tag: "Not here", \.hideGuests)
            row("Guests can invite others", has: caps?.guestsCanInvite == true, tag: "Google only", \.guestsInvite)
            row("Guests can edit", has: caps?.guestsCanModify == true, tag: "Google only", \.guestsModify)
        }
        .padding(.leading, 2)
    }

    private func row(_ label: String, has: Bool, tag: String, _ key: WritableKeyPath<MeetingOptions, Bool>) -> some View {
        HStack(spacing: 8) {
            Text(label).foregroundStyle(Color(has ? Theme.text : Theme.text3))
            if !has {
                Text(google && tag == "Not here" ? "Not on Google" : tag)
                    .font(.system(size: 10.5))
                    .foregroundStyle(Color(Theme.text3))
                    .padding(.horizontal, 5)
                    .background(Capsule().strokeBorder(Color(Theme.line), lineWidth: 1))
            }
            Spacer()
            Toggle("", isOn: Binding(get: { editor.draft.options[keyPath: key] }, set: { editor.draft.options[keyPath: key] = $0 }))
                .toggleStyle(.switch)
                .controlSize(.mini)
                .labelsHidden()
                .disabled(!has)
                .accessibilityLabel(label)
        }
        .frame(height: 22)
    }
}

/// Lays its views out in rows, wrapping to the next row when one is full.
struct FlowLayout: Layout {
    var spacing: CGFloat = 4

    func sizeThatFits(proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) -> CGSize {
        let rows = arrange(width: proposal.width ?? .infinity, subviews: subviews)
        let height = rows.last.map { $0.y + $0.height } ?? 0
        return CGSize(width: proposal.width ?? rows.map(\.width).max() ?? 0, height: height)
    }

    func placeSubviews(in bounds: CGRect, proposal: ProposedViewSize, subviews: Subviews, cache: inout ()) {
        for row in arrange(width: bounds.width, subviews: subviews) {
            var x = bounds.minX
            for i in row.items {
                let size = subviews[i].sizeThatFits(.unspecified)
                let width = i == row.items.last && i == subviews.count - 1 ? max(size.width, bounds.maxX - x) : size.width
                subviews[i].place(at: CGPoint(x: x, y: bounds.minY + row.y + (row.height - size.height) / 2), proposal: ProposedViewSize(width: width, height: size.height))
                x += size.width + spacing
            }
        }
    }

    private struct Row {
        var items: [Int] = []
        var y: CGFloat = 0
        var width: CGFloat = 0
        var height: CGFloat = 0
    }

    private func arrange(width: CGFloat, subviews: Subviews) -> [Row] {
        var rows: [Row] = [Row()]
        for i in subviews.indices {
            let size = subviews[i].sizeThatFits(.unspecified)
            if !rows[rows.count - 1].items.isEmpty && rows[rows.count - 1].width + spacing + size.width > width {
                let last = rows[rows.count - 1]
                rows.append(Row(y: last.y + last.height + spacing))
            }
            var row = rows[rows.count - 1]
            row.width += (row.items.isEmpty ? 0 : spacing) + size.width
            row.height = max(row.height, size.height)
            row.items.append(i)
            rows[rows.count - 1] = row
        }
        return rows
    }
}
