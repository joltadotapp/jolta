@preconcurrency import JoltaFFI
import AppKit
import ImageIO
import SwiftUI
import UniformTypeIdentifiers

/// A formatted text editor over the core's body format: bold (⌘B), italic
/// (⌘I), strikethrough (⇧⌘X) and links (⇧⌘K), from the Format menu.
/// Pasted text keeps only those styles; in a message, a pasted image goes
/// where the cursor is.
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

    var onShown: ((RichTextView) -> Void)?

    // SwiftUI adds the view to the window after `makeNSView`, at no fixed
    // point, so focusing from there can find no window yet.
    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        guard window != nil, let shown = onShown else { return }
        onShown = nil
        DispatchQueue.main.async { shown(self) }
    }

    /// Takes the keyboard with the cursor after `written`, the start of the
    /// body in the core's format: at the start when it is empty.
    func focus(after written: String, core: Core) {
        window?.makeFirstResponder(self)
        let shown = Self.attributed(core.bodySpans(body: written), base: base).string
        let found = (string as NSString).range(of: shown)
        let cursor = NSRange(location: found.location == NSNotFound ? 0 : NSMaxRange(found), length: 0)
        setSelectedRange(cursor)
        scrollRangeToVisible(cursor)
    }

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
            if let path = span.image { attrs = base; attrs[.attachment] = ImageAttachment(path: path, name: span.text) }
            out.append(NSAttributedString(string: span.image == nil ? span.text : "\u{FFFC}", attributes: attrs))
        }
        return out
    }

    /// The text as styled runs: bold and italic from the font, strikethrough, links.
    func spans() -> [TextSpan] {
        guard let storage = textStorage else { return [] }
        var out: [TextSpan] = []
        storage.enumerateAttributes(in: NSRange(location: 0, length: storage.length)) { attrs, range, _ in
            if let attachment = attrs[.attachment] {
                if let image = attachment as? ImageAttachment {
                    out.append(TextSpan(text: image.name, bold: false, italic: false, strike: false, link: nil, image: image.path))
                }
                return
            }
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

    /// Where pasted images and files go; only the message being written has one.
    var pasteTarget: PasteTarget?

    // Paste stays enabled for a clipboard holding only an image or a file.
    override var readablePasteboardTypes: [NSPasteboard.PasteboardType] {
        guard pasteTarget != nil else { return super.readablePasteboardTypes }
        let images = [UTType.png, .jpeg, .gif, .tiff, .heic].map { NSPasteboard.PasteboardType($0.identifier) }
        return super.readablePasteboardTypes + [.fileURL] + images
    }

    /// Pasted text keeps bold, italic, strikethrough and links, in Jolta's
    /// font and colours. In a message, a pasted image goes in at the cursor
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
