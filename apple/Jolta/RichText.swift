@preconcurrency import JoltaFFI
import AppKit
import SwiftUI

/// A formatted text editor over the core's body format: bold (⌘B), italic
/// (⌘I), strikethrough (⇧⌘X) and links (⇧⌘K), from the Format menu.
/// Pasted text keeps only those styles.
struct RichTextEditor: NSViewRepresentable {
    @Binding var text: String
    let core: Core
    var fontSize: CGFloat = 14
    /// Takes the keyboard when it appears, with the cursor at the start.
    var focusOnAppear = false
    /// The message being written, where snippets go (not a signature).
    var isMessageBody = false

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    func makeNSView(context: Context) -> NSScrollView {
        let view = RichTextView(frame: .zero)
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
        if isMessageBody { RichTextView.messageBody = view }
        view.typingAttributes = view.base
        view.delegate = context.coordinator
        let scroll = NSScrollView()
        scroll.drawsBackground = false
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        scroll.documentView = view
        context.coordinator.load(text, into: view)
        if focusOnAppear {
            DispatchQueue.main.async {
                view.window?.makeFirstResponder(view)
                view.setSelectedRange(NSRange(location: 0, length: 0))
                view.scrollRangeToVisible(NSRange(location: 0, length: 0))
            }
        }
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        context.coordinator.parent = self
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
            let spans = parent.core.bodySpans(body: text)
            view.textStorage?.setAttributedString(RichTextView.attributed(spans, base: view.base))
            lastText = text
        }

        func textDidChange(_ notification: Notification) {
            guard let view = notification.object as? RichTextView else { return }
            let body = parent.core.bodyMarkdown(spans: view.spans())
            lastText = body
            if parent.text != body { parent.text = body }
        }
    }
}

final class RichTextView: NSTextView {
    var base: [NSAttributedString.Key: Any] = [:]

    /// The editor that has the keyboard, for the Format menu.
    static var focused: RichTextView? { NSApp.keyWindow?.firstResponder as? RichTextView }
    /// The open message's body editor, which keeps its cursor while the
    /// snippet palette has the keyboard.
    static weak var messageBody: RichTextView?

    /// Puts a body (in the core's format) at the cursor, as one undoable edit.
    func insert(body: String, core: Core) {
        let text = Self.attributed(core.bodySpans(body: body), base: base)
        window?.makeFirstResponder(self)
        insertText(text, replacementRange: selectedRange())
    }

    static func baseAttributes(size: CGFloat) -> [NSAttributedString.Key: Any] {
        let paragraph = NSMutableParagraphStyle()
        paragraph.lineSpacing = 5
        return [.font: NSFont.systemFont(ofSize: size), .foregroundColor: Theme.text, .paragraphStyle: paragraph]
    }

    static func attributed(_ spans: [TextSpan], base: [NSAttributedString.Key: Any]) -> NSAttributedString {
        let out = NSMutableAttributedString()
        let font = base[.font] as? NSFont ?? .systemFont(ofSize: 14)
        for span in spans {
            var attrs = base
            var f = font
            if span.bold { f = NSFontManager.shared.convert(f, toHaveTrait: .boldFontMask) }
            if span.italic { f = NSFontManager.shared.convert(f, toHaveTrait: .italicFontMask) }
            attrs[.font] = f
            if span.strike { attrs[.strikethroughStyle] = NSUnderlineStyle.single.rawValue }
            if let link = span.link, let url = URL(string: link) { attrs[.link] = url }
            out.append(NSAttributedString(string: span.text, attributes: attrs))
        }
        return out
    }

    /// The text as styled runs: bold and italic from the font, strikethrough, links.
    func spans() -> [TextSpan] {
        guard let storage = textStorage else { return [] }
        var out: [TextSpan] = []
        storage.enumerateAttributes(in: NSRange(location: 0, length: storage.length)) { attrs, range, _ in
            let traits = (attrs[.font] as? NSFont).map { NSFontManager.shared.traits(of: $0) } ?? []
            let link: String? = switch attrs[.link] {
            case let url as URL: url.absoluteString
            case let s as String: s
            default: nil
            }
            out.append(TextSpan(text: (storage.string as NSString).substring(with: range),
                                bold: traits.contains(.boldFontMask), italic: traits.contains(.italicFontMask),
                                strike: (attrs[.strikethroughStyle] as? Int ?? 0) != 0, link: link, image: nil))
        }
        return out
    }

    enum Style { case bold, italic, strike }

    /// Turns a style on for the selection, or off when all of it has it.
    /// With nothing selected it applies to what is typed next.
    func toggle(_ style: Style) {
        let range = selectedRange()
        guard range.length > 0, let storage = textStorage else {
            typingAttributes = Self.toggled(style, in: typingAttributes, on: !Self.has(style, typingAttributes))
            return
        }
        var all = true
        storage.enumerateAttributes(in: range) { attrs, _, stop in
            if !Self.has(style, attrs) { all = false; stop.pointee = true }
        }
        guard shouldChangeText(in: range, replacementString: nil) else { return }
        storage.beginEditing()
        storage.enumerateAttributes(in: range) { attrs, sub, _ in
            storage.setAttributes(Self.toggled(style, in: attrs, on: !all), range: sub)
        }
        storage.endEditing()
        didChangeText()
    }

    private static func has(_ style: Style, _ attrs: [NSAttributedString.Key: Any]) -> Bool {
        let traits = (attrs[.font] as? NSFont).map { NSFontManager.shared.traits(of: $0) } ?? []
        return switch style {
        case .bold: traits.contains(.boldFontMask)
        case .italic: traits.contains(.italicFontMask)
        case .strike: (attrs[.strikethroughStyle] as? Int ?? 0) != 0
        }
    }

    private static func toggled(_ style: Style, in attrs: [NSAttributedString.Key: Any], on: Bool) -> [NSAttributedString.Key: Any] {
        var attrs = attrs
        let manager = NSFontManager.shared
        let font = attrs[.font] as? NSFont ?? .systemFont(ofSize: 14)
        switch style {
        case .bold: attrs[.font] = on ? manager.convert(font, toHaveTrait: .boldFontMask) : manager.convert(font, toNotHaveTrait: .boldFontMask)
        case .italic: attrs[.font] = on ? manager.convert(font, toHaveTrait: .italicFontMask) : manager.convert(font, toNotHaveTrait: .italicFontMask)
        case .strike: attrs[.strikethroughStyle] = on ? NSUnderlineStyle.single.rawValue : nil
        }
        return attrs
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

    /// Pasted text keeps bold, italic, strikethrough and links, in Jolta's
    /// font and colours.
    override func paste(_ sender: Any?) {
        guard let rich = NSPasteboard.general.readObjects(forClasses: [NSAttributedString.self])?.first as? NSAttributedString,
              rich.length > 0 else { return super.paste(sender) }
        let clean = NSMutableAttributedString()
        let font = base[.font] as? NSFont ?? .systemFont(ofSize: 14)
        rich.enumerateAttributes(in: NSRange(location: 0, length: rich.length)) { attrs, range, _ in
            var kept = base
            let traits = (attrs[.font] as? NSFont).map { NSFontManager.shared.traits(of: $0) } ?? []
            var f = font
            if traits.contains(.boldFontMask) { f = NSFontManager.shared.convert(f, toHaveTrait: .boldFontMask) }
            if traits.contains(.italicFontMask) { f = NSFontManager.shared.convert(f, toHaveTrait: .italicFontMask) }
            kept[.font] = f
            if let strike = attrs[.strikethroughStyle] as? Int, strike != 0 { kept[.strikethroughStyle] = strike }
            if let link = attrs[.link] { kept[.link] = link }
            clean.append(NSAttributedString(string: (rich.string as NSString).substring(with: range), attributes: kept))
        }
        insertText(clean, replacementRange: selectedRange())
    }
}
