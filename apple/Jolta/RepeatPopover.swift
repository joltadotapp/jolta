@preconcurrency import JoltaFFI
import SwiftUI

/// The repeat editor while it is open: the core's presets for the draft's
/// date, the row chosen and the Custom part. Every change asks the core for
/// the rule, its sentence, its next dates and whether the calendar holds it.
@MainActor
@Observable
final class RepeatPicker: Identifiable {
    enum Row: Hashable {
        case preset(RepeatPresetKind)
        case custom
    }

    private let core: Core
    private let draft: EventDraft
    let presets: [RepeatPreset]
    /// "On day 24" and "On the fourth Tuesday", the core's words.
    let monthDayLabel: String
    let monthWeekdayLabel: String
    var row: Row { didSet { update() } }
    var custom: CustomRepeat {
        didSet {
            changedCustom = true
            if row == .custom { update() } else { row = .custom }
        }
    }
    /// A rule Custom can't say, which stays as it is until the user picks a
    /// preset or changes Custom.
    private let kept: String?
    private var changedCustom = false
    private(set) var rule: String?
    private(set) var summary: RuleSummary
    /// Why the Custom part makes no rule.
    private(set) var failure: String?

    init(core: Core, draft: EventDraft) throws {
        let editor = try core.repeatEditor(draft: draft)
        self.core = core
        self.draft = draft
        presets = editor.presets
        monthDayLabel = editor.monthDayLabel
        monthWeekdayLabel = editor.monthWeekdayLabel
        custom = editor.custom
        kept = editor.customFits ? nil : draft.rule
        row = editor.selected.map(Row.preset) ?? .custom
        rule = draft.rule
        summary = editor.summary
    }

    /// The rows ↑ and ↓ move through: the presets the calendar holds, then Custom.
    var rows: [Row] { presets.filter { $0.refusal == nil }.map { .preset($0.kind) } + [.custom] }

    func move(_ by: Int) {
        let rows = rows
        let at = rows.firstIndex(of: row) ?? (by > 0 ? -1 : rows.count)
        row = rows[min(max(at + by, 0), rows.count - 1)]
    }

    var canFinish: Bool { failure == nil && summary.refusal == nil }

    private func update() {
        failure = nil
        switch row {
        case .preset(let kind):
            rule = presets.first { $0.kind == kind }?.rule
        case .custom:
            if let kept, !changedCustom {
                rule = kept
            } else {
                do {
                    rule = try core.customRule(draft: draft, custom: custom)
                } catch {
                    rule = nil
                    failure = "\(error)"
                }
            }
        }
        do {
            summary = try core.describeRule(draft: draft, rule: rule)
        } catch {
            summary = RuleSummary(text: nil, next: [], refusal: nil)
            failure = "\(error)"
        }
    }

    // MARK: Custom

    var weekdays: Set<UInt32> { Set(custom.weekdays) }

    /// A weekly rule keeps at least one day.
    func toggle(weekday: UInt32) {
        var days = weekdays
        if days.contains(weekday) { guard days.count > 1 else { return }; days.remove(weekday) } else { days.insert(weekday) }
        custom.weekdays = days.sorted()
    }

    func monthChoice(byWeekday: Bool) -> String {
        byWeekday ? monthWeekdayLabel : monthDayLabel
    }
}

/// The popover from the editor's Repeat row (⌘R): presets as radio rows,
/// greyed with the reason where the calendar can't hold them, Custom below
/// the list, and the sentence and next dates the rule makes.
struct RepeatPopover: View {
    @Bindable var picker: RepeatPicker
    let day: Date
    /// Where the arrow points, from the popover's top.
    let arrowY: CGFloat
    let cancel: () -> Void
    let done: () -> Void
    @FocusState private var listFocused: Bool

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("Repeat")
                .font(.system(size: 13, weight: .semibold))
                .padding(.horizontal, 8)
                .padding(.top, 2)
                .padding(.bottom, 8)
            list
            if picker.row == .custom { customPart }
            summary
            HStack(spacing: 8) {
                Spacer()
                EditorButton(title: "Cancel", key: "esc", action: cancel)
                    .keyboardShortcut(.cancelAction)
                EditorButton(title: "Done", key: "↩", primary: true, action: done)
                    .keyboardShortcut(.defaultAction)
                    .disabled(!picker.canFinish)
                    .accessibilityIdentifier("repeat-done")
            }
            .padding(.top, 14)
        }
        .padding(.horizontal, 12)
        .padding(.top, 12)
        .padding(.bottom, 14)
        .frame(width: 340)
        .font(.system(size: 12.5))
        .foregroundStyle(Color(Theme.text))
        .background(alignment: .topTrailing) {
            // The mockup's arrow: a square turned 45° half out of the right edge.
            RoundedRectangle(cornerRadius: 2)
                .fill(Color(Theme.raise))
                .frame(width: 12, height: 12)
                .rotationEffect(.degrees(45))
                .offset(x: 6, y: max(arrowY - 6, 14))
        }
        .background {
            RoundedRectangle(cornerRadius: 12)
                .fill(Color(Theme.raise))
                .shadow(color: .black.opacity(0.3), radius: 28, y: 16)
                .overlay(RoundedRectangle(cornerRadius: 12).strokeBorder(Color(Theme.line), lineWidth: 0.5))
        }
        .onAppear {
            // Off the editor's text fields, so ↑, ↓, Return and Esc reach
            // the calendar's key handler; it moves through the list.
            DispatchQueue.main.async { listFocused = true }
        }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("repeat-popover")
    }

    private var list: some View {
        VStack(alignment: .leading, spacing: 0) {
            ForEach(picker.presets, id: \.kind) { preset in
                RadioRow(title: preset.label, on: picker.row == .preset(preset.kind), focused: picker.row == .preset(preset.kind), reason: preset.refusal) {
                    picker.row = .preset(preset.kind)
                }
            }
            RadioRow(title: "Custom…", on: picker.row == .custom, focused: picker.row == .custom, reason: nil) { picker.row = .custom }
        }
        .focusable(interactions: .edit)
        .focusEffectDisabled()
        .focused($listFocused)
    }

    // MARK: Custom

    private var customPart: some View {
        VStack(alignment: .leading, spacing: 9) {
            HStack(spacing: 7) {
                Text("Every")
                NumberField(value: $picker.custom.every, id: "repeat-every")
                Menu {
                    ForEach([RepeatUnit.day, .week, .month, .year], id: \.self) { unit in
                        Button(unit.word(picker.custom.every)) { picker.custom.unit = unit }
                    }
                } label: {
                    Text(picker.custom.unit.word(picker.custom.every))
                }
                .menuStyle(.borderlessButton)
                .menuIndicator(.hidden)
                .frame(width: 76, alignment: .leading)
                .fieldBox(chevrons: true)
                .accessibilityIdentifier("repeat-unit")
            }
            if picker.custom.unit == .week {
                HStack(spacing: 5) {
                    ForEach(Array(zip(UInt32(1)...7, ["M", "T", "W", "T", "F", "S", "S"])), id: \.0) { weekday, letter in
                        WeekdayToggle(letter: letter, on: picker.weekdays.contains(weekday), name: Self.weekdayNames[Int(weekday) - 1]) {
                            picker.toggle(weekday: weekday)
                        }
                    }
                }
            }
            if picker.custom.unit == .month {
                VStack(alignment: .leading, spacing: 3) {
                    SmallRadio(on: !picker.custom.byWeekday) { picker.custom.byWeekday = false } label: { Text(picker.monthChoice(byWeekday: false)) }
                    SmallRadio(on: picker.custom.byWeekday) { picker.custom.byWeekday = true } label: { Text(picker.monthChoice(byWeekday: true)) }
                }
            }
            HStack(alignment: .top, spacing: 0) {
                Text("Ends").foregroundStyle(Color(Theme.text2)).padding(.top, 4).frame(width: 44, alignment: .leading)
                VStack(alignment: .leading, spacing: 3) {
                    SmallRadio(on: endDay != nil) { picker.custom.ends = .on(day: CalendarModel.dayKey(defaultEnd)) } label: {
                        Text("On")
                        EndDayChip(day: endDay ?? defaultEnd) { picker.custom.ends = .on(day: CalendarModel.dayKey($0)) }
                    }
                    SmallRadio(on: picker.custom.ends == .never) { picker.custom.ends = .never } label: { Text("Never") }
                    SmallRadio(on: endCount != nil) { picker.custom.ends = .after(count: endCount ?? 10) } label: {
                        Text("After")
                        NumberField(value: Binding(get: { endCount ?? 10 }, set: { picker.custom.ends = .after(count: $0) }), id: "repeat-count", height: 24)
                        Text("times")
                    }
                }
            }
            if let failure = picker.failure {
                ProblemLine(problem: DraftProblem(field: .repeat, text: failure, blocking: true))
            }
        }
        .padding(.leading, 31)
        .padding(.trailing, 8)
        .padding(.top, 10)
        .padding(.bottom, 2)
        .frame(maxWidth: .infinity, alignment: .leading)
        .overlay(alignment: .top) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
        .padding(.top, 6)
    }

    private var endDay: Date? {
        if case .on(let key) = picker.custom.ends { CalendarModel.date(key: key) } else { nil }
    }

    private var endCount: UInt32? {
        if case .after(let count) = picker.custom.ends { count } else { nil }
    }

    /// Four weeks after the start, as a first end to change.
    private var defaultEnd: Date { Calendar.current.date(byAdding: .day, value: 28, to: day) ?? day }

    private static let weekdayNames = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"]

    // MARK: Summary

    private var summary: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(picker.summary.text ?? "Does not repeat")
                .fixedSize(horizontal: false, vertical: true)
                .accessibilityIdentifier("repeat-summary")
            if !picker.summary.next.isEmpty {
                Text("Next: " + picker.summary.next.joined(separator: ", "))
                    .font(.system(size: 11.5))
                    .foregroundStyle(Color(Theme.text3))
            }
            if let refusal = picker.summary.refusal {
                ProblemLine(problem: DraftProblem(field: .repeat, text: refusal, blocking: true)).padding(.top, 4)
            }
        }
        .padding(.horizontal, 8)
        .padding(.top, 10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .overlay(alignment: .top) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
        .padding(.top, 12)
    }
}

/// The mockup's `.rd` in the popover's list: a radio circle and a label,
/// greyed with the reason under it when the calendar can't hold the choice.
private struct RadioRow: View {
    let title: String
    let on: Bool
    let focused: Bool
    let reason: String?
    let pick: () -> Void

    var body: some View {
        Button(action: pick) {
            VStack(alignment: .leading, spacing: 0) {
                HStack(spacing: 9) {
                    RadioCircle(on: on).opacity(reason == nil ? 1 : 0.5)
                    Text(title)
                    Spacer(minLength: 0)
                }
                if let reason {
                    Text(reason).font(.system(size: 11)).padding(.leading, 23).fixedSize(horizontal: false, vertical: true)
                }
            }
            .foregroundStyle(Color(reason == nil ? Theme.text : Theme.text3))
            .padding(.horizontal, 8)
            .padding(.vertical, reason == nil ? 0 : 3)
            .frame(minHeight: 24)
            .background { if focused { RoundedRectangle(cornerRadius: 6).fill(Color(Theme.fill)) } }
            .overlay(alignment: .leading) {
                if focused { Caret().padding(.leading, -11) }
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .disabled(reason != nil)
        .accessibilityLabel(reason.map { "\(title), \($0)" } ?? title)
        .accessibilityAddTraits(on ? .isSelected : [])
        .accessibilityIdentifier("repeat-row")
    }
}

/// The accent triangle beside the row the keyboard is on.
private struct Caret: View {
    var body: some View {
        Path { p in
            p.move(to: .zero)
            p.addLine(to: CGPoint(x: 7, y: 6))
            p.addLine(to: CGPoint(x: 0, y: 12))
            p.closeSubpath()
        }
        .fill(Color(Theme.accent))
        .frame(width: 7, height: 12)
    }
}

private struct RadioCircle: View {
    let on: Bool

    var body: some View {
        ZStack {
            Circle().fill(Color(Theme.raise))
            Circle().strokeBorder(Color(on ? Theme.text : Theme.text3), lineWidth: on ? 1.5 : 1)
            if on { Circle().fill(Color(Theme.text)).padding(3.5) }
        }
        .frame(width: 14, height: 14)
    }
}

/// A radio in the Custom part, whose label can hold a field.
private struct SmallRadio<Label: View>: View {
    let on: Bool
    let pick: () -> Void
    @ViewBuilder let label: Label

    var body: some View {
        HStack(spacing: 8) {
            Button(action: pick) { RadioCircle(on: on).contentShape(Rectangle()) }
                .buttonStyle(.plain)
                .accessibilityAddTraits(on ? .isSelected : [])
            label
        }
        .frame(minHeight: 24)
        .contentShape(Rectangle())
        .onTapGesture(perform: pick)
    }
}

/// One of the seven day toggles of a weekly rule.
private struct WeekdayToggle: View {
    let letter: String
    let on: Bool
    let name: String
    let toggle: () -> Void

    var body: some View {
        Button(action: toggle) {
            Text(letter)
                .font(.system(size: 11.5, weight: .semibold))
                .foregroundStyle(Color(on ? Theme.raise : Theme.text2))
                .frame(width: 26, height: 26)
                .background {
                    Circle().fill(Color(on ? Theme.text : Theme.raise))
                    if !on { Circle().strokeBorder(Color(Theme.line), lineWidth: 1) }
                }
                .contentShape(Circle())
        }
        .buttonStyle(.plain)
        .accessibilityLabel(name)
        .accessibilityAddTraits(on ? .isSelected : [])
    }
}

/// A whole number typed in a small box: "Every [2] weeks", "After [10] times".
private struct NumberField: View {
    @Binding var value: UInt32
    let id: String
    var height: CGFloat = 26
    @State private var text = ""
    @FocusState private var focused: Bool

    var body: some View {
        TextField("", text: $text)
            .textFieldStyle(.plain)
            .font(.system(size: 12, weight: .medium, design: .monospaced))
            .multilineTextAlignment(.center)
            .frame(width: 22)
            .fieldBox(focused: focused, height: height)
            .focused($focused)
            .onAppear { text = "\(value)" }
            .onChange(of: value) { if UInt32(text) != value { text = "\(value)" } }
            .onChange(of: text) {
                if let n = UInt32(text), n > 0, n != value { value = n }
            }
            .onChange(of: focused) { if !focused { text = "\(value)" } }
            .accessibilityIdentifier(id)
    }
}

/// "22 Dec 2026", which opens a month to pick the last day.
private struct EndDayChip: View {
    let day: Date
    let pick: (Date) -> Void
    @State private var open = false

    var body: some View {
        Button { open.toggle() } label: {
            Text(Self.format.string(from: day)).font(.system(size: 12)).fieldBox(focused: open, height: 24)
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier("repeat-until")
        .popover(isPresented: $open, arrowEdge: .bottom) {
            DatePicker("Ends on", selection: Binding(get: { day }, set: { pick($0); open = false }), displayedComponents: .date)
                .labelsHidden()
                .datePickerStyle(.graphical)
                .padding(12)
        }
    }

    private static let format: DateFormatter = {
        let f = DateFormatter()
        f.setLocalizedDateFormatFromTemplate("d MMM yyyy")
        return f
    }()
}

extension RepeatUnit {
    func word(_ count: UInt32) -> String {
        let one = count == 1
        return switch self {
        case .day: one ? "day" : "days"
        case .week: one ? "week" : "weeks"
        case .month: one ? "month" : "months"
        case .year: one ? "year" : "years"
        }
    }
}
