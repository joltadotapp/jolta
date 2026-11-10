@preconcurrency import JoltaFFI
import AppKit
import Carbon.HIToolbox
import ImageIO
import SwiftUI
import UniformTypeIdentifiers

/// A formatted text editor over the core's body format. The Format menu and
/// compose's formatting bar act on the editor that has the keyboard: inline
/// styles, alignment, indents, lists, quotes and links. Pasted text keeps
/// bold, italic, underline, strikethrough and links; in a message, a pasted
/// image goes where the cursor is.
struct RichTextEditor: NSViewRepresentable {
    @Binding var text: String
    let core: Core
    var fontSize: CGFloat = 14
    /// The message being written, where snippets go (not a signature).
    var isMessageBody = false
    /// Runs once the editor is in a window, where it and the fields beside
    /// it can take the keyboard.
    var onShown: ((RichTextView) -> Void)?

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    func makeNSView(context: Context) -> NSScrollView {
        Format.load(from: core)
        let view = RichTextView(frame: .zero)
        view.core = core
        view.isRichText = true
        view.allowsUndo = true
        view.importsGraphics = false
        view.drawsBackground = false
        view.isAutomaticLinkDetectionEnabled = true
        view.textContainerInset = .zero
        view.textContainer?.lineFragmentPadding = 0
        view.isVerticallyResizable = true
        view.autoresizingMask = .width
        view.textContainer?.widthTracksTextView = true
        view.base = RichTextView.baseAttributes(size: fontSize)
        if isMessageBody {
            RichTextView.messageBody = view
            view.setAccessibilityIdentifier("compose-body")
        }
        view.typingAttributes = view.base
        view.delegate = context.coordinator
        let scroll = NSScrollView()
        scroll.drawsBackground = false
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        scroll.documentView = view
        context.coordinator.load(text, into: view)
        view.onShown = onShown
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        context.coordinator.parent = self
        (scroll.documentView as? RichTextView)?.pasteTarget = context.environment.pasteTarget
        guard let view = scroll.documentView as? RichTextView, text != context.coordinator.lastText else { return }
        // Changed from outside (a signature swap, a snippet): reload, keeping the cursor.
        let cursor = view.selectedRange()
        context.coordinator.load(text, into: view)
        view.setSelectedRange(NSRange(location: min(cursor.location, view.string.utf16.count), length: 0))
    }

    @MainActor
    final class Coordinator: NSObject, NSTextViewDelegate {
        var parent: RichTextEditor
        /// The body the view shows, so edits made here don't reload it.
        var lastText: String?

        init(_ parent: RichTextEditor) { self.parent = parent }

        func load(_ text: String, into view: RichTextView) {
            view.textStorage?.setAttributedString(view.attributed(parent.core.bodyLines(body: text)))
            lastText = text
        }

        func textDidChange(_ notification: Notification) {
            guard let view = notification.object as? RichTextView else { return }
            let body = parent.core.bodyFromLines(lines: view.lines())
            lastText = body
            if parent.text != body { parent.text = body }
            view.publishState()
        }

        func textViewDidChangeSelection(_ notification: Notification) {
            (notification.object as? RichTextView)?.publishState()
        }

        func textView(_ textView: NSTextView, doCommandBy selector: Selector) -> Bool {
            (textView as? RichTextView)?.handleListKey(selector) ?? false
        }
    }
}

extension NSAttributedString.Key {
    /// The body format's font, size and colors for a run, beside the AppKit
    /// attributes that draw them.
    static let joltaFont = NSAttributedString.Key("app.jolta.font")
    static let joltaSize = NSAttributedString.Key("app.jolta.size")
    static let joltaColor = NSAttributedString.Key("app.jolta.color")
    static let joltaHighlight = NSAttributedString.Key("app.jolta.highlight")
}

/// The inline styles of a run of text, as the body format has them.
struct InlineStyle: Equatable {
    var bold = false
    var italic = false
    var underline = false
    var strike = false
    var font: TextFont?
    var size: TextSize?
    var color: TextColor?
    var highlight: TextHighlight?

    init() {}

    init(_ attrs: [NSAttributedString.Key: Any]) {
        let traits = (attrs[.font] as? NSFont).map { NSFontManager.shared.traits(of: $0) } ?? []
        bold = traits.contains(.boldFontMask)
        italic = traits.contains(.italicFontMask) || (attrs[.obliqueness] as? NSNumber)?.doubleValue ?? 0 > 0
        underline = (attrs[.underlineStyle] as? Int ?? 0) != 0
        strike = (attrs[.strikethroughStyle] as? Int ?? 0) != 0
        font = attrs[.joltaFont] as? TextFont
        size = attrs[.joltaSize] as? TextSize
        color = attrs[.joltaColor] as? TextColor
        highlight = attrs[.joltaHighlight] as? TextHighlight
    }

    init(_ span: TextSpan) {
        (bold, italic, underline, strike) = (span.bold, span.italic, span.underline, span.strike)
        (font, size, color, highlight) = (span.font, span.size, span.color, span.highlight)
    }

    func span(_ text: String, link: String?) -> TextSpan {
        TextSpan(text: text, bold: bold, italic: italic, strike: strike, underline: underline, font: font, size: size,
                 color: color, highlight: highlight, link: link, image: nil)
    }

    /// `attrs` drawn in this style. Links and the paragraph's style stay.
    func applied(to attrs: [NSAttributedString.Key: Any], base: [NSAttributedString.Key: Any]) -> [NSAttributedString.Key: Any] {
        var out = attrs
        let normal = base[.font] as? NSFont ?? .systemFont(ofSize: 14)
        let points = size.map { normal.pointSize * CGFloat(Format.px($0)) / CGFloat(Format.choices.normalPx) } ?? normal.pointSize
        let manager = NSFontManager.shared
        var f = font.flatMap { manager.font(withFamily: Format.family($0), traits: [], weight: 5, size: points) } ?? normal.withSize(points)
        if bold { f = manager.convert(f, toHaveTrait: .boldFontMask) }
        if italic { f = manager.convert(f, toHaveTrait: .italicFontMask) }
        out[.font] = f
        // A font with no italic face (Tahoma) slants instead.
        out[.obliqueness] = italic && !manager.traits(of: f).contains(.italicFontMask) ? 0.2 : nil
        out[.underlineStyle] = underline ? NSUnderlineStyle.single.rawValue : nil
        out[.strikethroughStyle] = strike ? NSUnderlineStyle.single.rawValue : nil
        out[.joltaFont] = font
        out[.joltaSize] = size
        out[.joltaColor] = color
        out[.joltaHighlight] = highlight
        // Highlights are light, so text on one is dark in any theme, as recipients see it.
        out[.foregroundColor] = color.map(Format.nsColor) ?? (highlight == nil ? base[.foregroundColor] : Format.ink)
        out[.backgroundColor] = highlight.map(Format.nsColor)
        return out
    }
}

final class RichTextView: NSTextView {
    var base: [NSAttributedString.Key: Any] = [:]
    var core: Core?

    /// The editor that has the keyboard, for the Format menu.
    static var focused: RichTextView? { NSApp.keyWindow?.firstResponder as? RichTextView }
    /// The open message's body editor, which keeps its cursor while the
    /// snippet palette has the keyboard.
    static weak var messageBody: RichTextView?

    var onShown: ((RichTextView) -> Void)?
    private var keyObservers: [NSObjectProtocol] = []

    // SwiftUI adds the view to the window after `makeNSView`, at no fixed
    // point, so focusing from there can find no window yet.
    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        keyObservers.forEach(NotificationCenter.default.removeObserver)
        keyObservers = []
        if let window {
            keyObservers = [NSWindow.didBecomeKeyNotification, NSWindow.didResignKeyNotification].map {
                NotificationCenter.default.addObserver(forName: $0, object: window, queue: .main) { _ in
                    MainActor.assumeIsolated { FormatState.shared.refreshEditing() }
                }
            }
        }
        guard window != nil, let shown = onShown else { return }
        onShown = nil
        DispatchQueue.main.async { shown(self) }
    }

    override func becomeFirstResponder() -> Bool {
        let became = super.becomeFirstResponder()
        if became { FormatState.shared.refreshEditing(); publishState() }
        return became
    }

    override func resignFirstResponder() -> Bool {
        let resigned = super.resignFirstResponder()
        if resigned { FormatState.shared.refreshEditing() }
        return resigned
    }

    /// ⇧⌘7 and ⇧⌘8 arrive as ⇧⌘& and ⇧⌘* (or another character, by
    /// layout), which no menu item matches, so they go by the key pressed.
    override func performKeyEquivalent(with event: NSEvent) -> Bool {
        if window?.firstResponder === self, event.modifierFlags.intersection(.deviceIndependentFlagsMask) == [.command, .shift] {
            switch Int(event.keyCode) {
            case kVK_ANSI_7: toggleBlock(.number); return true
            case kVK_ANSI_8: toggleBlock(.bullet); return true
            default: break
            }
        }
        return super.performKeyEquivalent(with: event)
    }

    /// Takes the keyboard with the cursor after `written`, the start of the
    /// body in the core's format: at the start when it is empty.
    func focus(after written: String, core: Core) {
        window?.makeFirstResponder(self)
        let shown = attributed(core.bodyLines(body: written)).string
        let found = (string as NSString).range(of: shown)
        let cursor = NSRange(location: found.location == NSNotFound ? 0 : NSMaxRange(found), length: 0)
        setSelectedRange(cursor)
        scrollRangeToVisible(cursor)
    }

    /// Puts a body (in the core's format) at the cursor, as one undoable edit.
    func insert(body: String, core: Core) {
        let text = attributed(core.bodyLines(body: body))
        window?.makeFirstResponder(self)
        insertText(text, replacementRange: selectedRange())
    }

    static func baseAttributes(size: CGFloat) -> [NSAttributedString.Key: Any] {
        let paragraph = NSMutableParagraphStyle()
        paragraph.lineSpacing = 5
        return [.font: NSFont.systemFont(ofSize: size), .foregroundColor: Theme.text, .paragraphStyle: paragraph]
    }

    // MARK: Lines

    /// How far one level of indent or list nesting moves a paragraph.
    static let indentStep: CGFloat = 24

    func paragraphStyle(indent: UInt8, align: TextAlign) -> NSParagraphStyle {
        let style = ((base[.paragraphStyle] as? NSParagraphStyle) ?? NSParagraphStyle()).mutableCopy() as! NSMutableParagraphStyle
        style.alignment = switch align {
        case .left: .natural
        case .center: .center
        case .right: .right
        }
        style.firstLineHeadIndent = CGFloat(indent) * Self.indentStep
        style.headIndent = style.firstLineHeadIndent
        return style
    }

    static func block(of style: NSParagraphStyle?) -> (indent: UInt8, align: TextAlign) {
        guard let style else { return (0, .left) }
        let align: TextAlign = switch style.alignment {
        case .center: .center
        case .right: .right
        default: .left
        }
        return (UInt8(max(0, min(Double(Format.choices.maxIndent), (style.firstLineHeadIndent / indentStep).rounded()))), align)
    }

    func attributed(_ lines: [TextLine]) -> NSAttributedString {
        let out = NSMutableAttributedString()
        for (i, line) in lines.enumerated() {
            var attrs = base
            attrs[.paragraphStyle] = paragraphStyle(indent: line.indent, align: line.align)
            for span in line.spans {
                if let path = span.image {
                    var picture = attrs
                    picture[.attachment] = ImageAttachment(path: path, name: span.text)
                    out.append(NSAttributedString(string: "\u{FFFC}", attributes: picture))
                    continue
                }
                var styled = InlineStyle(span).applied(to: attrs, base: base)
                if let link = span.link, let url = URL(string: link) { styled[.link] = url }
                out.append(NSAttributedString(string: span.text, attributes: styled))
            }
            if i < lines.count - 1 { out.append(NSAttributedString(string: "\n", attributes: attrs)) }
        }
        return out
    }

    /// Each paragraph's range, without its newline.
    func lineRanges() -> [NSRange] {
        let ns = string as NSString
        var out: [NSRange] = []
        var start = 0
        while true {
            let newline = ns.range(of: "\n", range: NSRange(location: start, length: ns.length - start))
            let end = newline.location == NSNotFound ? ns.length : newline.location
            out.append(NSRange(location: start, length: end - start))
            if newline.location == NSNotFound { return out }
            start = end + 1
        }
    }

    /// The paragraph's indent and alignment. An empty last paragraph has
    /// what is typed next.
    func block(of line: NSRange) -> (indent: UInt8, align: TextAlign) {
        guard let storage = textStorage else { return (0, .left) }
        let at = line.location < storage.length ? line.location : nil
        let style = at.map { storage.attribute(.paragraphStyle, at: $0, effectiveRange: nil) } ?? typingAttributes[.paragraphStyle]
        return Self.block(of: style as? NSParagraphStyle)
    }

    /// The text as paragraphs of styled runs: bold and italic from the font,
    /// the format's font, size and colors, underline, strikethrough, links.
    func lines() -> [TextLine] {
        lineRanges().map { line in
            let (indent, align) = block(of: line)
            return TextLine(indent: indent, align: align, spans: spans(in: line))
        }
    }

    func spans(in range: NSRange) -> [TextSpan] {
        guard let storage = textStorage, range.length > 0 else { return [] }
        var out: [TextSpan] = []
        storage.enumerateAttributes(in: range) { attrs, sub, _ in
            if let attachment = attrs[.attachment] {
                if let image = attachment as? ImageAttachment {
                    out.append(TextSpan(text: image.name, bold: false, italic: false, strike: false, underline: false, font: nil,
                                        size: nil, color: nil, highlight: nil, link: nil, image: image.path))
                }
                return
            }
            let link: String? = switch attrs[.link] {
            case let url as URL: url.absoluteString
            case let s as String: s
            default: nil
            }
            out.append(InlineStyle(attrs).span((storage.string as NSString).substring(with: sub), link: link))
        }
        return out
    }

    func text(of range: NSRange) -> String { (string as NSString).substring(with: range) }

    /// The paragraphs the selection touches, or the cursor's paragraph.
    func selectedLines() -> [NSRange] {
        let selection = selectedRange()
        let all = lineRanges()
        let touched = all.filter { line in
            selection.location <= NSMaxRange(line) && NSMaxRange(selection) >= line.location
                && !(selection.length > 0 && NSMaxRange(selection) == line.location && line.location > selection.location)
        }
        return touched.isEmpty ? Array(all.suffix(1)) : touched
    }

    // MARK: Inline styles

    enum Style { case bold, italic, underline, strike }

    /// Turns a style on for the selection, or off when all of it has it.
    /// With nothing selected it applies to what is typed next.
    func toggle(_ style: Style) {
        let path: WritableKeyPath<InlineStyle, Bool> = switch style {
        case .bold: \.bold
        case .italic: \.italic
        case .underline: \.underline
        case .strike: \.strike
        }
        let on = !allOfSelection { $0[keyPath: path] }
        restyle { $0[keyPath: path] = on }
    }

    func setFont(_ font: TextFont?) { restyle { $0.font = font } }
    func setSize(_ size: TextSize?) { restyle { $0.size = size } }
    func setColor(_ color: TextColor?) { restyle { $0.color = color } }
    func setHighlight(_ highlight: TextHighlight?) { restyle { $0.highlight = highlight } }

    /// One size up or down: Small, Normal, Large, Huge.
    func stepSize(_ by: Int) {
        let steps: [TextSize?] = [.small, nil, .large, .huge]
        let now = steps.firstIndex(of: currentStyle().size) ?? 1
        setSize(steps[max(0, min(steps.count - 1, now + by))])
    }

    /// Removes every inline style from the selection, keeping links and images.
    func clearFormatting() {
        guard let core else { return }
        let range = selectedRange()
        guard range.length > 0, let storage = textStorage else {
            typingAttributes = InlineStyle().applied(to: typingAttributes, base: base)
            return publishState()
        }
        guard shouldChangeText(in: range, replacementString: nil) else { return }
        storage.beginEditing()
        storage.enumerateAttributes(in: range) { attrs, sub, _ in
            guard attrs[.attachment] == nil else { return }
            let span = InlineStyle(attrs).span(text(of: sub), link: nil)
            let cleared = core.clearFormatting(spans: [span]).first.map(InlineStyle.init) ?? InlineStyle()
            storage.setAttributes(cleared.applied(to: attrs, base: base), range: sub)
        }
        storage.endEditing()
        didChangeText()
    }

    /// Asks for an address and links the selection to it, or inserts the
    /// address as a link when nothing is selected.
    func addLink() {
        let range = selectedRange()
        let selected = (string as NSString).substring(with: range)
        let alert = NSAlert()
        alert.messageText = "Add Link"
        let field = NSTextField(frame: NSRect(x: 0, y: 0, width: 300, height: 24))
        field.placeholderString = "https://"
        if selected.hasPrefix("http") { field.stringValue = selected }
        alert.accessoryView = field
        alert.addButton(withTitle: "Add")
        alert.addButton(withTitle: "Cancel")
        alert.window.initialFirstResponder = field
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        var address = field.stringValue.trimmingCharacters(in: .whitespaces)
        guard !address.isEmpty else { return }
        if !address.contains(":") { address = (address.contains("@") ? "mailto:" : "https://") + address }
        guard let url = URL(string: address), let storage = textStorage else { return }
        window?.makeFirstResponder(self)
        if range.length == 0 {
            var attrs = typingAttributes
            attrs[.link] = url
            insertText(NSAttributedString(string: field.stringValue, attributes: attrs), replacementRange: range)
        } else if shouldChangeText(in: range, replacementString: nil) {
            storage.addAttribute(.link, value: url, range: range)
            didChangeText()
        }
    }

    /// The parts of `range` outside list and quote markers. A marker stays
    /// plain: inside `**` it would be text.
    private func styleable(_ range: NSRange) -> [NSRange] {
        lineRanges().compactMap { line in
            let m = Int(marker(of: line).length)
            let piece = NSIntersectionRange(NSRange(location: line.location + m, length: line.length - m), range)
            return piece.length > 0 ? piece : nil
        }
    }

    private func allOfSelection(_ test: (InlineStyle) -> Bool) -> Bool {
        let pieces = styleable(selectedRange())
        guard !pieces.isEmpty, let storage = textStorage else { return test(InlineStyle(typingAttributes)) }
        var all = true
        for piece in pieces where all {
            storage.enumerateAttributes(in: piece) { attrs, _, stop in
                if attrs[.attachment] == nil, !test(InlineStyle(attrs)) { all = false; stop.pointee = true }
            }
        }
        return all
    }

    /// Changes the inline style of the selection, or of what is typed next.
    private func restyle(_ change: (inout InlineStyle) -> Void) {
        let range = selectedRange()
        let pieces = styleable(range)
        guard !pieces.isEmpty, let storage = textStorage else {
            var style = InlineStyle(typingAttributes)
            change(&style)
            typingAttributes = style.applied(to: typingAttributes, base: base)
            return publishState()
        }
        guard shouldChangeText(in: range, replacementString: nil) else { return }
        storage.beginEditing()
        for piece in pieces {
            storage.enumerateAttributes(in: piece) { attrs, sub, _ in
                guard attrs[.attachment] == nil else { return }
                var style = InlineStyle(attrs)
                change(&style)
                storage.setAttributes(style.applied(to: attrs, base: base), range: sub)
            }
        }
        storage.endEditing()
        didChangeText()
    }

    /// The style at the start of the selection, or what is typed next.
    func currentStyle() -> InlineStyle {
        guard selectedRange().length > 0, let storage = textStorage, let first = styleable(selectedRange()).first else {
            return InlineStyle(typingAttributes)
        }
        return InlineStyle(storage.attributes(at: first.location, effectiveRange: nil))
    }

    // MARK: Paragraphs

    /// Aligns the selected paragraphs.
    func align(_ align: TextAlign) {
        reblock { _, _ in (nil, align) }
    }

    /// Indents the selected paragraphs one level, or outdents them; a list
    /// item nests deeper or less deep.
    func indent(_ by: Int) {
        let most = Int(Format.choices.maxIndent)
        reblock { indent, _ in (UInt8(max(0, min(most, Int(indent) + by))), nil) }
        renumber()
    }

    /// Changes the indent or alignment of the selected paragraphs; nil keeps it.
    private func reblock(_ change: (UInt8, TextAlign) -> (UInt8?, TextAlign?)) {
        guard let storage = textStorage else { return }
        let lines = selectedLines()
        let union = NSUnionRange(lines[0], lines[lines.count - 1])
        let styled = NSRange(location: union.location, length: min(storage.length - union.location, union.length + 1))
        if styled.length > 0, shouldChangeText(in: styled, replacementString: nil) {
            storage.beginEditing()
            for line in lines {
                let (indent, align) = block(of: line)
                let (newIndent, newAlign) = change(indent, align)
                let range = NSRange(location: line.location, length: min(storage.length - line.location, line.length + 1))
                guard range.length > 0 else { continue }
                storage.addAttribute(.paragraphStyle, value: paragraphStyle(indent: newIndent ?? indent, align: newAlign ?? align), range: range)
            }
            storage.endEditing()
            didChangeText()
        }
        let (indent, align) = Self.block(of: typingAttributes[.paragraphStyle] as? NSParagraphStyle)
        let (newIndent, newAlign) = change(indent, align)
        typingAttributes[.paragraphStyle] = paragraphStyle(indent: newIndent ?? indent, align: newAlign ?? align)
        publishState()
    }

    func marker(of line: NSRange) -> LineMarker {
        core?.bodyLineMarker(text: text(of: line)) ?? LineMarker(kind: .paragraph, length: 0)
    }

    /// Makes the selected paragraphs bulleted, numbered or quoted, or plain
    /// again when all of them already are.
    func toggleBlock(_ kind: LineKind) {
        let lines = selectedLines()
        let all = lines.allSatisfy { marker(of: $0).kind == kind }
        rewriteMarkers(lines) { _, _ in
            switch (all, kind) {
            case (true, _), (_, .paragraph): ""
            case (_, .bullet): "• "
            case (_, .number): "1. "
            case (_, .quote): "> "
            }
        }
        renumber()
    }

    /// Replaces each paragraph's marker with what `newMarker` gives, as one
    /// undoable change; the selection keeps to the same text.
    private func rewriteMarkers(_ lines: [NSRange], _ newMarker: (NSRange, LineMarker) -> String?) {
        guard let storage = textStorage, !lines.isEmpty else { return }
        let union = NSUnionRange(lines[0], lines[lines.count - 1])
        let rewritten = NSMutableAttributedString(attributedString: storage.attributedSubstring(from: union))
        let selection = selectedRange()
        var shift = (start: 0, end: 0)
        for line in lines.reversed() {
            let old = marker(of: line)
            guard let new = newMarker(line, old), new != text(of: NSRange(location: line.location, length: Int(old.length))) else { continue }
            var attrs = base
            attrs[.paragraphStyle] = paragraphStyle(indent: block(of: line).indent, align: block(of: line).align)
            let local = NSRange(location: line.location - union.location, length: Int(old.length))
            rewritten.replaceCharacters(in: local, with: NSAttributedString(string: new, attributes: attrs))
            let delta = (new as NSString).length - Int(old.length)
            if line.location < selection.location || line.location == selection.location && selection.length == 0 { shift.start += delta }
            if line.location < NSMaxRange(selection) || selection.length == 0 && line.location == selection.location { shift.end += delta }
        }
        guard rewritten.string != text(of: union), shouldChangeText(in: union, replacementString: rewritten.string) else { return }
        storage.replaceCharacters(in: union, with: rewritten)
        didChangeText()
        let start = max(union.location, selection.location + shift.start)
        setSelectedRange(NSRange(location: start, length: max(0, NSMaxRange(selection) + shift.end - start)))
    }

    /// Numbers the numbered items around the selection 1, 2, 3 within each
    /// list and level.
    func renumber() {
        let all = lineRanges()
        let touched = selectedLines()
        guard let first = all.firstIndex(of: touched[0]), let last = all.firstIndex(of: touched[touched.count - 1]) else { return }
        let isItem = { (line: NSRange) in [.bullet, .number].contains(self.marker(of: line).kind) }
        var lo = first, hi = last
        while lo > 0, isItem(all[lo - 1]) { lo -= 1 }
        while hi < all.count - 1, isItem(all[hi + 1]) { hi += 1 }
        var counts: [Int] = []
        var numbers: [NSRange: Int] = [:]
        for line in all[lo...hi] {
            let kind = marker(of: line).kind
            guard kind == .bullet || kind == .number else { counts = []; continue }
            let level = Int(block(of: line).indent)
            counts = Array(counts.prefix(level + 1)) + Array(repeating: 0, count: max(0, level + 1 - counts.count))
            counts[level] = kind == .number ? counts[level] + 1 : 0
            if kind == .number { numbers[line] = counts[level] }
        }
        rewriteMarkers(Array(all[lo...hi])) { line, _ in numbers[line].map { "\($0). " } }
    }

    /// Return continues a list, and ends it on an empty item; Tab and ⇧Tab
    /// nest an item; Delete right after a marker removes it.
    func handleListKey(_ selector: Selector) -> Bool {
        let selection = selectedRange()
        guard selection.length == 0 else { return false }
        let line = selectedLines()[0]
        let m = marker(of: line)
        let afterMarker = selection.location >= line.location + Int(m.length) && m.length > 0
        let item = m.kind == .bullet || m.kind == .number
        switch selector {
        case #selector(insertNewline(_:)) where item && afterMarker:
            if line.length == Int(m.length) {
                if block(of: line).indent > 0 { indent(-1) } else { rewriteMarkers([line]) { _, _ in "" } }
                return true
            }
            let current = text(of: NSRange(location: line.location, length: Int(m.length)))
            let next = m.kind == .number ? "\((Int(current.dropLast(2)) ?? 0) + 1). " : current
            var attrs = base
            attrs[.paragraphStyle] = paragraphStyle(indent: block(of: line).indent, align: block(of: line).align)
            let typing = typingAttributes
            insertText(NSAttributedString(string: "\n" + next, attributes: attrs), replacementRange: selection)
            typingAttributes = typing
            if m.kind == .number { renumber() }
            return true
        case #selector(insertTab(_:)) where item:
            indent(1)
            return true
        case #selector(insertBacktab(_:)) where item:
            indent(-1)
            return true
        case #selector(deleteBackward(_:)) where m.length > 0 && selection.location == line.location + Int(m.length):
            rewriteMarkers([line]) { _, _ in "" }
            renumber()
            return true
        default:
            return false
        }
    }

    /// Tells the formatting bar and the Format menu what the selection has.
    func publishState() {
        guard window?.firstResponder === self || RichTextView.messageBody === self else { return }
        let range = selectedRange()
        var style = currentStyle()
        if range.length > 0 {
            style.bold = allOfSelection(\.bold)
            style.italic = allOfSelection(\.italic)
            style.underline = allOfSelection(\.underline)
            style.strike = allOfSelection(\.strike)
        }
        let line = selectedLines()[0]
        let (indent, align) = block(of: line)
        var link = false
        if let storage = textStorage, range.location < storage.length {
            link = storage.attribute(.link, at: range.location, effectiveRange: nil) != nil
        }
        FormatState.shared.show(.init(style: style, kind: marker(of: line).kind, indent: indent, align: align, link: link))
    }

    /// Where pasted images and files go; only the message being written has one.
    var pasteTarget: PasteTarget?

    // Paste stays enabled for a clipboard holding only an image or a file.
    override var readablePasteboardTypes: [NSPasteboard.PasteboardType] {
        guard pasteTarget != nil else { return super.readablePasteboardTypes }
        let images = [UTType.png, .jpeg, .gif, .tiff, .heic].map { NSPasteboard.PasteboardType($0.identifier) }
        return super.readablePasteboardTypes + [.fileURL] + images
    }

    /// Pasted text keeps bold, italic, underline, strikethrough and links,
    /// in Jolta's font and colours. In a message, a pasted image goes in at the cursor
    /// and other files become attachments.
    override func paste(_ sender: Any?) {
        let board = NSPasteboard.general
        if let target = pasteTarget, pasteFilesOrImage(from: board, into: target) { return }
        guard let rich = board.readObjects(forClasses: [NSAttributedString.self])?.first as? NSAttributedString,
              rich.length > 0 else { return super.paste(sender) }
        let clean = NSMutableAttributedString()
        let font = base[.font] as? NSFont ?? .systemFont(ofSize: 14)
        rich.enumerateAttributes(in: NSRange(location: 0, length: rich.length)) { attrs, range, _ in
            if let attachment = attrs[.attachment] as? NSTextAttachment {
                // Pictures in copied rich text, such as one copied from this editor.
                if let target = pasteTarget, let data = attachment.fileWrapper?.regularFileContents,
                   let image = imageAttachment(data, name: attachment.fileWrapper?.preferredFilename, target: target) {
                    clean.append(NSAttributedString(string: "\u{FFFC}", attributes: base.merging([.attachment: image]) { $1 }))
                }
                return
            }
            var kept = base
            kept[.paragraphStyle] = typingAttributes[.paragraphStyle] ?? base[.paragraphStyle]
            let traits = (attrs[.font] as? NSFont).map { NSFontManager.shared.traits(of: $0) } ?? []
            var f = font
            if traits.contains(.boldFontMask) { f = NSFontManager.shared.convert(f, toHaveTrait: .boldFontMask) }
            if InlineStyle(attrs).italic { f = NSFontManager.shared.convert(f, toHaveTrait: .italicFontMask) }
            kept[.font] = f
            if let strike = attrs[.strikethroughStyle] as? Int, strike != 0 { kept[.strikethroughStyle] = strike }
            if let underline = attrs[.underlineStyle] as? Int, underline != 0, attrs[.link] == nil { kept[.underlineStyle] = underline }
            if let link = attrs[.link] { kept[.link] = link }
            clean.append(NSAttributedString(string: (rich.string as NSString).substring(with: range), attributes: kept))
        }
        insertText(clean, replacementRange: selectedRange())
    }
}

/// What the message being written does with pasted images and files.
struct PasteTarget {
    let core: Core
    /// Files that aren't images, to attach.
    let attach: ([URL]) -> Void
    /// Why a paste couldn't go in.
    let failed: (String) -> Void
}

extension EnvironmentValues {
    @Entry var pasteTarget: PasteTarget?
}

extension RichTextView {
    /// A copied file, or an image copied from a screenshot, a browser or
    /// Preview. False when the clipboard holds text to paste instead: a
    /// copied table or page also carries a picture of itself.
    fileprivate func pasteFilesOrImage(from board: NSPasteboard, into target: PasteTarget) -> Bool {
        let files = board.readObjects(forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL] ?? []
        if !files.isEmpty {
            var others: [URL] = []
            let pasted = NSMutableAttributedString()
            for file in files {
                guard UTType(filenameExtension: file.pathExtension)?.conforms(to: .image) ?? false,
                      let data = try? Data(contentsOf: file), let image = Self.mailImage(data) else {
                    others.append(file)
                    continue
                }
                guard let attachment = keep(image, name: file.lastPathComponent, target: target) else { continue }
                pasted.append(NSAttributedString(string: "\u{FFFC}", attributes: base.merging([.attachment: attachment]) { $1 }))
            }
            if pasted.length > 0 { insertText(pasted, replacementRange: selectedRange()) }
            if !others.isEmpty { target.attach(others) }
            return true
        }
        let text = board.string(forType: .string)?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        let justALink = !text.contains(where: \.isWhitespace) && URL(string: text)?.scheme?.hasPrefix("http") == true
        guard text.isEmpty || justALink,
              let type = board.availableType(from: [.png, NSPasteboard.PasteboardType(UTType.jpeg.identifier),
                                                    NSPasteboard.PasteboardType(UTType.gif.identifier), .tiff,
                                                    NSPasteboard.PasteboardType(UTType.heic.identifier)]),
              let data = board.data(forType: type) else { return false }
        if let attachment = imageAttachment(data, name: nil, target: target) {
            insertText(NSAttributedString(string: "\u{FFFC}", attributes: base.merging([.attachment: attachment]) { $1 }),
                       replacementRange: selectedRange())
        }
        return true
    }

    fileprivate func imageAttachment(_ data: Data, name: String?, target: PasteTarget) -> ImageAttachment? {
        guard let image = Self.mailImage(data) else {
            target.failed("That image can't be read.")
            return nil
        }
        return keep(image, name: name, target: target)
    }

    /// Keeps an image for the message as a file the core sends from.
    private func keep(_ image: (Data, String), name: String?, target: PasteTarget) -> ImageAttachment? {
        let (bytes, ext) = image
        let stem = name.map { ($0 as NSString).deletingPathExtension }.flatMap { $0.isEmpty ? nil : $0 } ?? "Pasted image"
        do {
            return ImageAttachment(path: try target.core.savePastedImage(data: bytes), name: "\(stem).\(ext)")
        } catch {
            target.failed("\(error)")
            return nil
        }
    }

    /// PNG, JPEG and GIF stay as they are. Other formats (TIFF from Preview,
    /// HEIC photos) become PNG, or JPEG when that PNG would be over 5 MB and
    /// the image has no transparency, keeping their orientation and density.
    static func mailImage(_ data: Data) -> (Data, String)? {
        guard let source = CGImageSourceCreateWithData(data as CFData, nil),
              let type = CGImageSourceGetType(source).flatMap({ UTType($0 as String) }) else { return nil }
        for (kept, ext) in [(UTType.png, "png"), (.jpeg, "jpg"), (.gif, "gif")] where type == kept { return (data, ext) }
        let properties = CGImageSourceCopyPropertiesAtIndex(source, 0, nil) as? [CFString: Any] ?? [:]
        let side = max(properties[kCGImagePropertyPixelWidth] as? Int ?? 0, properties[kCGImagePropertyPixelHeight] as? Int ?? 0)
        let options: [CFString: Any] = [kCGImageSourceCreateThumbnailFromImageAlways: true,
                                        kCGImageSourceCreateThumbnailWithTransform: true,
                                        kCGImageSourceThumbnailMaxPixelSize: max(side, 1)]
        guard let image = CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary) else { return nil }
        let density = properties.filter { $0.key == kCGImagePropertyDPIWidth || $0.key == kCGImagePropertyDPIHeight }
        func encode(_ type: UTType, _ extra: [CFString: Any] = [:]) -> Data? {
            let out = NSMutableData()
            guard let destination = CGImageDestinationCreateWithData(out, type.identifier as CFString, 1, nil) else { return nil }
            CGImageDestinationAddImage(destination, image, density.merging(extra) { $1 } as CFDictionary)
            return CGImageDestinationFinalize(destination) ? out as Data : nil
        }
        guard let png = encode(.png) else { return nil }
        let opaque = properties[kCGImagePropertyHasAlpha] as? Bool != true
        if png.count > 5 << 20, opaque, let jpeg = encode(.jpeg, [kCGImageDestinationLossyCompressionQuality: 0.85]) {
            return (jpeg, "jpg")
        }
        return (png, "png")
    }
}

/// An image in a message, shown from the file the core sends it from.
nonisolated final class ImageAttachment: NSTextAttachment {
    let path: String
    let name: String

    init(path: String, name: String) {
        self.path = path
        self.name = name
        super.init(data: nil, ofType: nil)
        // Copying it within the editor carries the picture along.
        if let data = FileManager.default.contents(atPath: path) {
            let file = FileWrapper(regularFileWithContents: data)
            file.preferredFilename = name
            fileWrapper = file
        }
        image = NSImage(contentsOfFile: path)
    }

    required init?(coder: NSCoder) { nil }

    // At the size it had on screen, narrowed to fit the line.
    override func attachmentBounds(for attributes: [NSAttributedString.Key: Any], location: any NSTextLocation,
                                   textContainer: NSTextContainer?, proposedLineFragment: CGRect, position: CGPoint) -> CGRect {
        fitted(to: proposedLineFragment.width)
    }

    override func attachmentBounds(for textContainer: NSTextContainer?, proposedLineFragment lineFrag: NSRect,
                                   glyphPosition position: CGPoint, characterIndex charIndex: Int) -> NSRect {
        fitted(to: lineFrag.width)
    }

    private func fitted(to width: CGFloat) -> CGRect {
        guard let size = image?.size, size.width > 0 else { return CGRect(x: 0, y: 0, width: 16, height: 16) }
        let scale = width > 0 ? min(1, width / size.width) : 1
        return CGRect(x: 0, y: 0, width: size.width * scale, height: size.height * scale)
    }
}

extension Core {
    /// A body's first line of text, for a list.
    func bodyPreview(_ body: String) -> String {
        bodyLines(body: body).lazy.map { $0.spans.map(\.text).joined() }.first { !$0.isEmpty } ?? ""
    }
}
