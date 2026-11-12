@preconcurrency import JoltaFFI
import SwiftUI

/// The body format's fonts, sizes and colors as the editor shows them. The
/// values come from the core, which also writes them into sent mail.
@MainActor
enum Format {
    static var choices = FormatChoices(fonts: [], sizes: [], colors: [], highlights: [], ink: "#1f1f1f", normalPx: 14, maxIndent: 4)

    static func load(from core: Core) {
        if choices.fonts.isEmpty { choices = core.formatChoices() }
    }

    static func family(_ font: TextFont) -> String { choices.fonts.first { $0.font == font }?.family ?? "" }
    static func px(_ size: TextSize) -> UInt32 { choices.sizes.first { $0.size == size }?.px ?? choices.normalPx }
    static func nsColor(_ color: TextColor) -> NSColor { hex(choices.colors.first { $0.color == color }?.hex) }
    static func nsColor(_ highlight: TextHighlight) -> NSColor { hex(choices.highlights.first { $0.highlight == highlight }?.hex) }
    static var ink: NSColor { hex(choices.ink) }

    private static func hex(_ value: String?) -> NSColor {
        guard let value, let n = UInt32(value.dropFirst(), radix: 16) else { return .labelColor }
        return NSColor(srgbRed: CGFloat(n >> 16 & 0xFF) / 255, green: CGFloat(n >> 8 & 0xFF) / 255, blue: CGFloat(n & 0xFF) / 255, alpha: 1)
    }

    static let fonts: [TextFont?] = [nil, .arial, .verdana, .tahoma, .trebuchet, .georgia, .times, .courier]
    static let sizes: [TextSize?] = [.small, nil, .large, .huge]
    static let colors: [TextColor] = [.red, .orange, .green, .teal, .blue, .purple, .pink, .gray]
    static let highlights: [TextHighlight] = [.yellow, .green, .blue, .pink, .orange, .purple]

    static func name(_ font: TextFont?) -> String {
        switch font {
        case nil: "Default"
        case .arial: "Arial"
        case .verdana: "Verdana"
        case .tahoma: "Tahoma"
        case .trebuchet: "Trebuchet MS"
        case .georgia: "Georgia"
        case .times: "Times New Roman"
        case .courier: "Courier New"
        }
    }

    static func name(_ size: TextSize?) -> String {
        switch size {
        case .small: "Small"
        case nil: "Normal"
        case .large: "Large"
        case .huge: "Huge"
        }
    }

    static func name(_ color: TextColor) -> String {
        switch color {
        case .red: "Red"
        case .orange: "Orange"
        case .green: "Green"
        case .teal: "Teal"
        case .blue: "Blue"
        case .purple: "Purple"
        case .pink: "Pink"
        case .gray: "Gray"
        }
    }

    static func name(_ highlight: TextHighlight) -> String {
        switch highlight {
        case .yellow: "Yellow"
        case .green: "Green"
        case .blue: "Blue"
        case .pink: "Pink"
        case .orange: "Orange"
        case .purple: "Purple"
        }
    }
}

/// What the selection in the editor with the keyboard has, for the
/// formatting bar's lit buttons and the Format menu.
@MainActor @Observable
final class FormatState {
    static let shared = FormatState()

    struct Snapshot: Equatable {
        var style = InlineStyle()
        var kind: LineKind = .paragraph
        var indent: UInt8 = 0
        var align: TextAlign = .left
        var link = false
    }

    private(set) var now = Snapshot()
    /// A text editor has the keyboard in the key window, so the Format
    /// menu's keys are its (⌘U underlines instead of unsubscribing).
    private(set) var editing = false

    func show(_ snapshot: Snapshot) {
        if snapshot != now { now = snapshot }
    }

    func refreshEditing() {
        DispatchQueue.main.async {
            let editing = NSApp.keyWindow?.firstResponder is RichTextView
            if editing != self.editing { self.editing = editing }
        }
    }
}

/// Every formatting command, for the Format menu, the formatting bar and
/// their tooltips.
@MainActor
enum FormatTool: CaseIterable {
    case bold, italic, underline, strike
    case bigger, smaller
    case alignLeft, alignCenter, alignRight
    case bullets, numbers, outdent, indent, quote
    case link, clear

    var title: String {
        switch self {
        case .bold: "Bold"
        case .italic: "Italic"
        case .underline: "Underline"
        case .strike: "Strikethrough"
        case .bigger: "Bigger"
        case .smaller: "Smaller"
        case .alignLeft: "Align Left"
        case .alignCenter: "Center"
        case .alignRight: "Align Right"
        case .bullets: "Bulleted List"
        case .numbers: "Numbered List"
        case .outdent: "Decrease Indentation"
        case .indent: "Increase Indentation"
        case .quote: "Quote"
        case .link: "Add Link…"
        case .clear: "Clear Formatting"
        }
    }

    var icon: String {
        switch self {
        case .bold: "bold"
        case .italic: "italic"
        case .underline: "underline"
        case .strike: "strikethrough"
        case .bigger: "textformat.size.larger"
        case .smaller: "textformat.size.smaller"
        case .alignLeft: "text.alignleft"
        case .alignCenter: "text.aligncenter"
        case .alignRight: "text.alignright"
        case .bullets: "list.bullet"
        case .numbers: "list.number"
        case .outdent: "decrease.indent"
        case .indent: "increase.indent"
        case .quote: "text.quote"
        case .link: "link"
        case .clear: "eraser"
        }
    }

    /// macOS's usual key for it where there is one (TextEdit's alignment,
    /// Mail's quote), else Gmail's (lists, clear formatting).
    var shortcut: KeyboardShortcut {
        switch self {
        case .bold: KeyboardShortcut("b")
        case .italic: KeyboardShortcut("i")
        case .underline: KeyboardShortcut("u")
        case .strike: KeyboardShortcut("x", modifiers: [.command, .shift])
        case .bigger: KeyboardShortcut("+")
        case .smaller: KeyboardShortcut("-")
        case .alignLeft: KeyboardShortcut("{")
        case .alignCenter: KeyboardShortcut("|")
        case .alignRight: KeyboardShortcut("}")
        case .bullets: KeyboardShortcut("8", modifiers: [.command, .shift])
        case .numbers: KeyboardShortcut("7", modifiers: [.command, .shift])
        case .outdent: KeyboardShortcut("[")
        case .indent: KeyboardShortcut("]")
        case .quote: KeyboardShortcut("'")
        case .link: KeyboardShortcut("k", modifiers: [.command, .shift])
        case .clear: KeyboardShortcut("\\")
        }
    }

    var keys: String {
        switch self {
        case .bold: "⌘B"
        case .italic: "⌘I"
        case .underline: "⌘U"
        case .strike: "⇧⌘X"
        case .bigger: "⌘+"
        case .smaller: "⌘-"
        case .alignLeft: "⌘{"
        case .alignCenter: "⌘|"
        case .alignRight: "⌘}"
        case .bullets: "⇧⌘8"
        case .numbers: "⇧⌘7"
        case .outdent: "⌘["
        case .indent: "⌘]"
        case .quote: "⌘'"
        case .link: "⇧⌘K"
        case .clear: "⌘\\"
        }
    }

    var id: String { "format-\(self)" }

    func perform(on editor: RichTextView) {
        switch self {
        case .bold: editor.toggle(.bold)
        case .italic: editor.toggle(.italic)
        case .underline: editor.toggle(.underline)
        case .strike: editor.toggle(.strike)
        case .bigger: editor.stepSize(1)
        case .smaller: editor.stepSize(-1)
        case .alignLeft: editor.align(.left)
        case .alignCenter: editor.align(.center)
        case .alignRight: editor.align(.right)
        case .bullets: editor.toggleBlock(.bullet)
        case .numbers: editor.toggleBlock(.number)
        case .outdent: editor.indent(-1)
        case .indent: editor.indent(1)
        case .quote: editor.toggleBlock(.quote)
        case .link: editor.addLink()
        case .clear: editor.clearFormatting()
        }
    }

    func isOn(_ s: FormatState.Snapshot) -> Bool {
        switch self {
        case .bold: s.style.bold
        case .italic: s.style.italic
        case .underline: s.style.underline
        case .strike: s.style.strike
        case .alignLeft: s.align == .left
        case .alignCenter: s.align == .center
        case .alignRight: s.align == .right
        case .bullets: s.kind == .bullet
        case .numbers: s.kind == .number
        case .quote: s.kind == .quote
        case .link: s.link
        case .bigger, .smaller, .outdent, .indent, .clear: false
        }
    }
}

/// The Format menu: every tool with its key, for the message and for
/// signatures, plus showing or hiding compose's formatting bar.
struct FormatMenu: View {
    @AppStorage(AppModel.key("showsFormatBar")) private var showsBar = true
    private var state: FormatState { FormatState.shared }

    var body: some View {
        Menu("Font") {
            ForEach(Format.fonts, id: \.self) { font in
                Toggle(Format.name(font), isOn: chosen(state.now.style.font == font) { $0.setFont(font) })
            }
        }
        .disabled(!state.editing)
        Menu("Size") {
            ForEach(Format.sizes, id: \.self) { size in
                Toggle(Format.name(size), isOn: chosen(state.now.style.size == size) { $0.setSize(size) })
            }
            Divider()
            item(.bigger)
            item(.smaller)
        }
        .disabled(!state.editing)
        Divider()
        item(.bold)
        item(.italic)
        item(.underline)
        item(.strike)
        Menu("Text Color") {
            Toggle("None", isOn: chosen(state.now.style.color == nil) { $0.setColor(nil) })
            ForEach(Format.colors, id: \.self) { color in
                Toggle(Format.name(color), isOn: chosen(state.now.style.color == color) { $0.setColor(color) })
            }
        }
        .disabled(!state.editing)
        Menu("Highlight") {
            Toggle("None", isOn: chosen(state.now.style.highlight == nil) { $0.setHighlight(nil) })
            ForEach(Format.highlights, id: \.self) { highlight in
                Toggle(Format.name(highlight), isOn: chosen(state.now.style.highlight == highlight) { $0.setHighlight(highlight) })
            }
        }
        .disabled(!state.editing)
        Divider()
        item(.alignLeft)
        item(.alignCenter)
        item(.alignRight)
        Divider()
        item(.bullets)
        item(.numbers)
        item(.quote)
        item(.outdent)
        item(.indent)
        Divider()
        item(.link)
        item(.clear)
        Divider()
        Button(showsBar ? "Hide Formatting Bar" : "Show Formatting Bar") { showsBar.toggle() }
            .keyboardShortcut("t", modifiers: [.command, .option])
    }

    /// ⌘U is Unsubscribe in the list, so it is Underline's only while an
    /// editor has the keyboard.
    private func item(_ tool: FormatTool) -> some View {
        Button(tool.title) { RichTextView.focused.map(tool.perform) }
            .keyboardShortcut(tool == .underline && !state.editing ? nil : tool.shortcut)
            .disabled(!state.editing)
    }

    private func chosen(_ on: Bool, _ choose: @escaping (RichTextView) -> Void) -> Binding<Bool> {
        Binding(get: { on }, set: { _ in RichTextView.focused.map(choose) })
    }
}

/// Compose's formatting bar above the message. It drops the less used tools
/// into a More menu as the pane narrows.
struct FormatBar: View {
    @AppStorage(AppModel.key("showsFormatBar")) private var showsBar = true
    private var state: FormatState { FormatState.shared }

    private enum Part { case font, styles, strike, colors, highlight, align, alignMenu, lists, indents, extras }

    private static let layouts: [[Part]] = [
        [.font, .styles, .strike, .colors, .highlight, .align, .lists, .indents, .extras],
        [.font, .styles, .strike, .colors, .highlight, .alignMenu, .lists, .indents],
        [.styles, .strike, .colors, .highlight, .lists, .indents],
        [.styles, .colors, .lists],
    ]

    var body: some View {
        ViewThatFits(in: .horizontal) {
            ForEach(Self.layouts.indices, id: \.self) { i in
                row(Self.layouts[i])
            }
        }
        // The first control's glyph sits 6 points inside it.
        .padding(.horizontal, Theme.gutter - 6)
        .frame(height: 36)
        .frame(maxWidth: .infinity, alignment: .leading)
        .overlay(alignment: .bottom) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
        .accessibilityElement(children: .contain)
        .accessibilityIdentifier("format-bar")
    }

    private func row(_ groups: [Part]) -> some View {
        HStack(spacing: 1) {
            ForEach(Array(groups.enumerated()), id: \.offset) { i, group in
                if i > 0, group != .strike, group != .highlight { separator }
                switch group {
                case .font: fontMenus
                case .styles: tools([.bold, .italic, .underline])
                case .strike: tools([.strike])
                case .colors: ColorButton(kind: .text)
                case .highlight: ColorButton(kind: .highlight)
                case .align: tools([.alignLeft, .alignCenter, .alignRight])
                case .alignMenu: alignMenu
                case .lists: tools([.bullets, .numbers])
                case .indents: tools([.outdent, .indent])
                case .extras: tools([.quote, .link, .clear])
                }
            }
            if groups != Self.layouts[0] { separator; more(missing: groups) }
            Spacer(minLength: 8)
            BarButton(icon: "chevron.up", help: "Hide formatting  ⌥⌘T", id: "format-hide") { showsBar = false }
        }
        .fixedSize(horizontal: true, vertical: false)
    }

    private var separator: some View {
        Rectangle().fill(Color(Theme.line)).frame(width: 1, height: 16).padding(.horizontal, 4)
    }

    private func tools(_ list: [FormatTool]) -> some View {
        ForEach(list, id: \.self) { tool in
            BarButton(icon: tool.icon, help: "\(tool.title)  \(tool.keys)", id: tool.id, on: tool.isOn(state.now)) { run(tool.perform) }
        }
    }

    private var fontMenus: some View {
        HStack(spacing: 1) {
            BarMenu(title: Format.name(state.now.style.font), width: 82, help: "Font", id: "format-font") { fontItems }
            BarMenu(title: Format.name(state.now.style.size), width: 62, help: "Size  ⌘+ ⌘-", id: "format-size") { sizeItems }
        }
    }

    private var alignMenu: some View {
        let current: FormatTool = switch state.now.align {
        case .left: .alignLeft
        case .center: .alignCenter
        case .right: .alignRight
        }
        return BarMenu(icon: current.icon, width: 34, help: "Alignment", id: "format-align") { alignItems }
    }

    @ViewBuilder private var fontItems: some View {
        ForEach(Format.fonts, id: \.self) { font in
            Toggle(Format.name(font), isOn: choice(state.now.style.font == font) { $0.setFont(font) })
        }
    }

    @ViewBuilder private var sizeItems: some View {
        ForEach(Format.sizes, id: \.self) { size in
            Toggle(Format.name(size), isOn: choice(state.now.style.size == size) { $0.setSize(size) })
        }
    }

    @ViewBuilder private var alignItems: some View {
        ForEach([FormatTool.alignLeft, .alignCenter, .alignRight], id: \.self) { tool in
            Toggle(tool.title, isOn: choice(tool.isOn(state.now), tool.perform))
        }
    }

    /// Everything the row leaves out, in the order the full row has it.
    private func more(missing groups: [Part]) -> some View {
        BarMenu(icon: "ellipsis", width: 30, help: "More formatting", id: "format-more") {
            if !groups.contains(.font) {
                Menu("Font") { fontItems }
                Menu("Size") { sizeItems }
            }
            if !groups.contains(.strike) { menuItem(.strike) }
            if !groups.contains(.highlight) {
                Menu("Highlight") {
                    Toggle("None", isOn: choice(state.now.style.highlight == nil) { $0.setHighlight(nil) })
                    ForEach(Format.highlights, id: \.self) { h in
                        Toggle(Format.name(h), isOn: choice(state.now.style.highlight == h) { $0.setHighlight(h) })
                    }
                }
            }
            if !groups.contains(.align) && !groups.contains(.alignMenu) { Menu("Alignment") { alignItems } }
            if !groups.contains(.indents) {
                menuItem(.outdent)
                menuItem(.indent)
            }
            Divider()
            menuItem(.quote)
            menuItem(.link)
            menuItem(.clear)
        }
    }

    private func menuItem(_ tool: FormatTool) -> some View {
        Button(tool.title) { run(tool.perform) }
    }

    private func choice(_ on: Bool, _ choose: @escaping (RichTextView) -> Void) -> Binding<Bool> {
        Binding(get: { on }, set: { _ in run(choose) })
    }

    /// Acts on the message body, taking the keyboard back to it.
    private func run(_ action: (RichTextView) -> Void) {
        guard let body = RichTextView.messageBody else { return }
        body.window?.makeFirstResponder(body)
        action(body)
    }
}

private struct BarButton: View {
    let icon: String
    let help: String
    let id: String
    var on = false
    let action: () -> Void
    @State private var hovering = false

    var body: some View {
        Button(action: action) {
            Image(systemName: icon)
                .font(.system(size: 12, weight: on ? .semibold : .regular))
                .foregroundStyle(Color(on ? Theme.text : Theme.text2))
                .frame(width: 26, height: 26)
                .background(RoundedRectangle(cornerRadius: 6).fill(Color(on ? Theme.fill : hovering ? Theme.hover : .clear)))
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
        .help(help)
        .accessibilityLabel(help)
        .accessibilityIdentifier(id)
        .accessibilityValue(on ? "on" : "off")
    }
}

private struct BarMenu<Content: View>: View {
    var title: String?
    var icon: String?
    let width: CGFloat
    let help: String
    let id: String
    @ViewBuilder let content: () -> Content

    init(title: String? = nil, icon: String? = nil, width: CGFloat, help: String, id: String, @ViewBuilder content: @escaping () -> Content) {
        (self.title, self.icon, self.width, self.help, self.id, self.content) = (title, icon, width, help, id, content)
    }

    var body: some View {
        Menu {
            content()
        } label: {
            if let title {
                Text(title).font(.system(size: 12)).lineLimit(1)
            } else if let icon {
                Image(systemName: icon).font(.system(size: 12))
            }
        }
        .menuStyle(.borderlessButton)
        .menuIndicator(title == nil ? .hidden : .visible)
        .foregroundStyle(Color(Theme.text2))
        .frame(width: width, height: 26)
        .help(help)
        .accessibilityIdentifier(id)
    }
}

/// The text color or highlight: a letter over the current color, opening a
/// row of swatches and None.
private struct ColorButton: View {
    enum Kind { case text, highlight }
    let kind: Kind
    @State private var open = false
    @State private var hovering = false
    private var state: FormatState { FormatState.shared }

    private var current: NSColor? {
        switch kind {
        case .text: state.now.style.color.map(Format.nsColor)
        case .highlight: state.now.style.highlight.map(Format.nsColor)
        }
    }

    private var name: String { kind == .text ? "Text color" : "Highlight" }

    var body: some View {
        Button { open.toggle() } label: {
            VStack(spacing: 1.5) {
                Image(systemName: kind == .text ? "character" : "highlighter")
                    .font(.system(size: 11.5, weight: .medium))
                    .foregroundStyle(Color(Theme.text2))
                RoundedRectangle(cornerRadius: 1)
                    .fill(current.map { Color(nsColor: $0) } ?? Color(Theme.line))
                    .frame(width: 14, height: 3)
            }
            .frame(width: 28, height: 26)
            .background(RoundedRectangle(cornerRadius: 6).fill(Color(hovering || open ? Theme.hover : .clear)))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .onHover { hovering = $0 }
        .help(name)
        .accessibilityLabel(name)
        .accessibilityIdentifier(kind == .text ? "format-color" : "format-highlight")
        .popover(isPresented: $open, arrowEdge: .bottom) { swatches }
    }

    private var swatches: some View {
        let names: [(String, NSColor?, () -> Void)] = switch kind {
        case .text: [("None", nil, { run { $0.setColor(nil) } })] + Format.colors.map { c in (Format.name(c), Format.nsColor(c), { run { $0.setColor(c) } }) }
        case .highlight: [("None", nil, { run { $0.setHighlight(nil) } })] + Format.highlights.map { h in (Format.name(h), Format.nsColor(h), { run { $0.setHighlight(h) } }) }
        }
        // The swatches are 18 points and 6 apart; each takes clicks on 24.
        return HStack(spacing: 0) {
            ForEach(names.indices, id: \.self) { i in
                let (label, color, pick) = names[i]
                let chosen = color.map { current == $0 } ?? (current == nil)
                Button(action: pick) {
                    ZStack {
                        Circle().fill(color.map { Color(nsColor: $0) } ?? Color(Theme.reader))
                        Circle().strokeBorder(Color(chosen ? Theme.text : Theme.line), lineWidth: chosen ? 1.5 : 1)
                        if color == nil { Image(systemName: "slash.circle").font(.system(size: 9)).foregroundStyle(Color(Theme.text3)) }
                    }
                    .frame(width: 18, height: 18)
                    .frame(width: Theme.controlSmall, height: Theme.controlSmall).contentShape(Rectangle())
                }
                .buttonStyle(.plain)
                .help(label)
                .accessibilityLabel("\(name) \(label)")
                .accessibilityAddTraits(chosen ? .isSelected : [])
                .accessibilityIdentifier("\(kind == .text ? "format-color" : "format-highlight")-\(label.lowercased())")
            }
        }
        .padding(7)
    }

    private func run(_ action: (RichTextView) -> Void) {
        open = false
        guard let body = RichTextView.messageBody else { return }
        body.window?.makeFirstResponder(body)
        action(body)
    }
}
