@preconcurrency import JoltaFFI
import Contacts
import SwiftUI

/// A To, Cc or Bcc field that suggests people as you type. ↑ ↓ pick one,
/// ↩ or ⇥ takes it, esc closes the list.
struct RecipientField: View {
    @Binding var text: String
    let core: Core
    let account: Int64?
    /// Every recipient field as typed, so people already on the message aren't suggested.
    let recipients: String
    let active: Bool
    @State private var suggestions: [PersonInfo] = []
    @State private var selected = 0
    /// Esc closes the list until the next edit.
    @State private var dismissed = false

    private var showing: Bool { active && !dismissed && !suggestions.isEmpty }

    /// The address being typed: whatever follows the last comma.
    private var typed: String {
        String(text.split(separator: ",", omittingEmptySubsequences: false).last ?? "").trimmingCharacters(in: .whitespaces)
    }

    var body: some View {
        TextField("", text: $text)
            .onChange(of: text) {
                dismissed = false
                refresh()
            }
            .onChange(of: active) { refresh() }
            .onKeyPress(.downArrow) { move(1) }
            .onKeyPress(.upArrow) { move(-1) }
            .onKeyPress(.return) { take() }
            .onKeyPress(.tab) { take() }
            .onKeyPress(.escape) {
                guard showing else { return .ignored }
                dismissed = true
                return .handled
            }
            .overlay(alignment: .topLeading) {
                if showing { list.offset(y: 30) }
            }
    }

    private var list: some View {
        VStack(spacing: 0) {
            ForEach(Array(suggestions.enumerated()), id: \.offset) { i, person in
                SuggestionRow(person: person, on: i == selected) {
                    accept(person)
                } forget: {
                    try? core.forgetAddress(address: person.address)
                    refresh()
                }
            }
        }
        .padding(5)
        .frame(width: 380)
        .menuPanel()
    }

    private func refresh() {
        let word = typed
        guard active, !word.isEmpty else {
            suggestions = []
            return
        }
        suggestions = (try? core.suggestAddresses(typed: word, accountId: account, recipients: recipients)) ?? []
        selected = 0
    }

    private func move(_ step: Int) -> KeyPress.Result {
        guard showing else { return .ignored }
        selected = (selected + step + suggestions.count) % suggestions.count
        return .handled
    }

    private func take() -> KeyPress.Result {
        guard showing, suggestions.indices.contains(selected) else { return .ignored }
        accept(suggestions[selected])
        return .handled
    }

    /// Replaces what was being typed with the person, ready for the next one.
    private func accept(_ person: PersonInfo) {
        var parts = text.split(separator: ",", omittingEmptySubsequences: false).map { $0.trimmingCharacters(in: .whitespaces) }
        parts.removeLast()
        parts.removeAll(where: \.isEmpty)
        parts.append(Self.entry(person))
        text = parts.joined(separator: ", ") + ", "
        suggestions = []
    }

    /// `Name <address>`, quoting names that would confuse an address list.
    static func entry(_ person: PersonInfo) -> String {
        guard !person.name.isEmpty else { return person.address }
        let special = CharacterSet(charactersIn: ",;<>\"@()[]:\\")
        let name = person.name.rangeOfCharacter(from: special) == nil
            ? person.name
            : "\"" + person.name.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\"") + "\""
        return "\(name) <\(person.address)>"
    }
}

struct SuggestionRow: View {
    let person: PersonInfo
    let on: Bool
    let choose: () -> Void
    let forget: () -> Void
    @State private var hovering = false

    var body: some View {
        HStack(spacing: 8) {
            if !person.name.isEmpty {
                Text(person.name).foregroundStyle(Color(Theme.text)).lineLimit(1).layoutPriority(1)
            }
            Text(person.address).foregroundStyle(Color(Theme.text3)).lineLimit(1)
            Spacer(minLength: 8)
            if hovering || on {
                Button(action: forget) {
                    Image(systemName: "xmark").font(.system(size: 9.5, weight: .semibold))
                }
                .buttonStyle(.plain)
                .foregroundStyle(Color(Theme.text3))
                .help("Don't suggest \(person.address) until you write to it again")
            }
        }
        .font(.system(size: 13))
        .padding(.horizontal, 10)
        .frame(height: 30)
        .selectedRow(on, hovering: hovering)
        .contentShape(Rectangle())
        .onTapGesture(perform: choose)
        .onHover { hovering = $0 }
    }
}

/// The Mac's Contacts as a second source of suggestions, when switched on
/// in Settings › People. Contacts never leave the Mac.
enum AddressBook {
    static let key = "SuggestContacts"

    static var enabled: Bool { UserDefaults.standard.bool(forKey: key) }

    /// Asks for access the first time. Returns whether Jolta may read Contacts.
    static func requestAccess() async -> Bool {
        (try? await CNContactStore().requestAccess(for: .contacts)) ?? false
    }

    /// Hands every contact with an email address to the core, or nothing
    /// when the source is off or access was refused.
    static func load(into core: Core) {
        guard enabled, CNContactStore.authorizationStatus(for: .contacts) == .authorized else {
            core.setAddressBook(people: [])
            return
        }
        Task.detached(priority: .utility) {
            let people = read()
            await MainActor.run { core.setAddressBook(people: people) }
        }
    }

    private nonisolated static func read() -> [PersonInfo] {
        let keys = [CNContactGivenNameKey, CNContactFamilyNameKey, CNContactOrganizationNameKey, CNContactEmailAddressesKey] as [CNKeyDescriptor]
        var people: [PersonInfo] = []
        try? CNContactStore().enumerateContacts(with: CNContactFetchRequest(keysToFetch: keys)) { contact, _ in
            let full = [contact.givenName, contact.familyName].filter { !$0.isEmpty }.joined(separator: " ")
            let name = full.isEmpty ? contact.organizationName : full
            for email in contact.emailAddresses {
                people.append(PersonInfo(name: name, address: email.value as String))
            }
        }
        return people
    }
}
