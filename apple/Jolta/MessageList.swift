@preconcurrency import JoltaFFI
import AppKit
import SwiftUI

/// The message list: a view-based `NSTableView` that asks the core for rows
/// page by page as it scrolls, so a 100k-message mailbox costs a few pages.
struct MessageList: NSViewRepresentable {
    let model: AppModel

    func makeCoordinator() -> Coordinator { Coordinator(model: model) }

    func makeNSView(context: Context) -> NSScrollView {
        let table = KeyTableView()
        table.headerView = nil
        table.style = .plain
        table.rowHeight = Theme.rowHeight
        table.intercellSpacing = .zero
        table.backgroundColor = .clear
        table.selectionHighlightStyle = .regular
        table.focusRingType = .none
        table.allowsEmptySelection = true
        table.allowsMultipleSelection = true
        // Letters are commands; type-select would jump to a row instead.
        table.allowsTypeSelect = false
        table.usesAutomaticRowHeights = false
        let column = NSTableColumn(identifier: .init("thread"))
        column.resizingMask = .autoresizingMask
        table.addTableColumn(column)
        table.columnAutoresizingStyle = .uniformColumnAutoresizingStyle
        table.dataSource = context.coordinator
        table.delegate = context.coordinator
        table.target = context.coordinator
        table.action = #selector(Coordinator.clicked)
        table.doubleAction = #selector(Coordinator.openSelected)
        KeyRouter.shared.handler = { [weak coordinator = context.coordinator] key in coordinator?.handle(key) ?? false }
        context.coordinator.table = table

        let scroll = NSScrollView()
        scroll.documentView = table
        scroll.contentInsets = NSEdgeInsets(top: 2, left: 0, bottom: 8, right: 0)
        scroll.hasVerticalScroller = true
        scroll.drawsBackground = false
        scroll.automaticallyAdjustsContentInsets = false
        context.coordinator.reload(full: true)
        if UserDefaults.standard.bool(forKey: "JoltaScrollTest") {
            DispatchQueue.main.asyncAfter(deadline: .now() + 1) { context.coordinator.scrollTest() }
        }
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        let c = context.coordinator
        _ = model.listRevision
        if c.lastQuery != model.query {
            c.reload(full: true)
        } else if c.lastRevision != model.listRevision {
            c.reload(full: false, resorted: c.lastSort != model.sort)
        }
        c.lastRevision = model.listRevision
        if let table = c.table, c.focusRequest != model.focusListRequest || c.wantsFocus {
            c.focusRequest = model.focusListRequest
            c.wantsFocus = table.window == nil
            table.window?.makeFirstResponder(table)
        }
    }

    @MainActor
    final class Coordinator: NSObject, NSTableViewDataSource, NSTableViewDelegate {
        let model: AppModel
        weak var table: KeyTableView?
        var lastQuery = ""
        var lastSort = ListSort(key: .date, reversed: false)
        var lastRevision = -1
        var wantsFocus = true
        var focusRequest = 0
        private var reportedLaunch = false
        private var count = 0
        private var pages: [Int: [ThreadRow]] = [:]
        private let pageSize = 100

        init(model: AppModel) { self.model = model }

        /// A new list starts at the top. A changed list keeps the selected
        /// threads selected wherever they moved. When the user's own change
        /// took them out (archived, moved), the cursor stays at the same
        /// height, on the next thread. Mail that comes or goes by itself never
        /// gets the cursor, which would open it and mark it read.
        func reload(full: Bool, resorted: Bool = false) {
            guard let table else { return }
            let userChanged = model.userChangedList
            model.userChangedList = false
            pages.removeAll(keepingCapacity: true)
            let previous = count
            let anchor = table.selectedRowIndexes.first ?? 0
            count = model.count()
            lastQuery = model.query
            lastSort = model.sort
            if count == 0, model.selectedThreadID != nil || !model.selectedThreads.isEmpty { model.closeThread() }
            let rows = count
            DispatchQueue.main.async { [model] in if model.listCount != rows { model.listCount = rows } }
            if full {
                table.reloadData()
                // A thread opened before the list was built, such as from a
                // notification while calendar mode showed, keeps its row.
                let row = model.selectedThreadID.flatMap { model.rows(of: [$0]).first } ?? 0
                table.scrollRowToVisible(row)
                if count > 0 { table.selectRowIndexes([row], byExtendingSelection: false) }
                if !reportedLaunch, count > 0 {
                    reportedLaunch = true
                    DispatchQueue.main.async {
                        AppModel.timing(String(format: "launch to first rows drawn: %.0f ms (%d threads)", AppModel.sinceLaunch, self.count))
                    }
                }
                return
            }
            if previous != count || resorted {
                table.noteNumberOfRowsChanged()
                table.reloadData()
            } else {
                let visible = table.rows(in: table.visibleRect)
                if visible.length > 0 {
                    table.reloadData(forRowIndexes: IndexSet(integersIn: visible.location..<min(count, visible.location + visible.length)),
                                     columnIndexes: [0])
                }
                // Reloading cells leaves row views alone, and they draw the unread dot.
                table.enumerateAvailableRowViews { view, row in
                    (view as? ThreadRowView)?.unread = (self.row(at: row)?.unreadCount ?? 0) > 0
                }
            }
            guard count > 0 else { return }
            var kept = model.rows(of: model.selectedThreads)
            if kept.isEmpty {
                guard userChanged else {
                    table.deselectAll(nil)
                    if model.selectedThreadID != nil || !model.selectedThreads.isEmpty { model.closeThread() }
                    return
                }
                kept = [min(anchor, count - 1)]
            }
            if kept != table.selectedRowIndexes {
                table.selectRowIndexes(kept, byExtendingSelection: false)
            }
            if resorted || previous != count, let first = kept.first { table.scrollRowToVisible(first) }
            // The same row can come back changed, say starred, without a selection change.
            if table.selectedRow >= 0, let r = row(at: table.selectedRow), r != model.cursorRow { model.cursorRow = r }
        }

        func row(at index: Int) -> ThreadRow? {
            let p = index / pageSize
            if pages[p] == nil {
                pages[p] = model.page(offset: p * pageSize, limit: pageSize)
            }
            let rows = pages[p]!
            let i = index - p * pageSize
            return i < rows.count ? rows[i] : nil
        }

        func numberOfRows(in tableView: NSTableView) -> Int { count }

        func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
            let cell = (tableView.makeView(withIdentifier: ThreadCell.id, owner: nil) as? ThreadCell) ?? ThreadCell()
            if let r = self.row(at: row) { cell.show(r) }
            cell.mark = tableView.isRowSelected(row) ? (tableView.numberOfSelectedRows > 1 ? .selected : .cursor) : .none
            return cell
        }

        func tableView(_ tableView: NSTableView, rowViewForRow row: Int) -> NSTableRowView? {
            let view = (tableView.makeView(withIdentifier: ThreadRowView.id, owner: nil) as? ThreadRowView) ?? ThreadRowView()
            view.unread = (self.row(at: row)?.unreadCount ?? 0) > 0
            return view
        }

        /// The reader follows the cursor, so j and k read through the list.
        /// Shift-click and Command-click select several threads instead.
        func tableViewSelectionDidChange(_ notification: Notification) {
            guard let table, table.selectedRow >= 0, let r = row(at: table.selectedRow) else { return }
            table.enumerateAvailableRowViews { view, _ in (view as? ThreadRowView)?.updateMark() }
            model.cursor = (table.selectedRow, count)
            model.cursorRow = r
            if table.numberOfSelectedRows > 1 {
                model.select(model.threadIDs(rows: table.selectedRowIndexes))
            } else if model.selectedThreadID != r.threadId {
                model.open(threadID: r.threadId)
            }
        }

        /// Scrolls through the whole list in screen-sized steps, one step per
        /// run-loop turn like real scrolling, drawing each one, and writes
        /// per-step timings next to the store. Run with `-JoltaScrollTest YES`.
        func scrollTest() {
            guard let table, let clip = table.enclosingScrollView?.contentView else { return }
            let step = clip.bounds.height
            let total = CGFloat(count) * table.rowHeight
            let clock = ContinuousClock()
            var times: [Double] = []
            func next(_ y: CGFloat) {
                guard y < total else { return finish(times) }
                let t = clock.measure {
                    clip.scroll(to: NSPoint(x: 0, y: y))
                    table.enclosingScrollView?.reflectScrolledClipView(clip)
                    table.window?.displayIfNeeded()
                }
                times.append(Double(t.components.attoseconds) / 1e15 + Double(t.components.seconds) * 1000)
                DispatchQueue.main.async { next(y + step) }
            }
            next(0)
        }

        private func finish(_ samples: [Double]) {
            let times = samples.sorted()
            let p = { (q: Double) in times[min(times.count - 1, Int(Double(times.count - 1) * q))] }
            let report = String(format: "rows %d, steps %d, p50 %.2f ms, p95 %.2f ms, p99 %.2f ms, max %.2f ms\n",
                                count, times.count, p(0.5), p(0.95), p(0.99), times.last ?? 0)
            try? report.write(toFile: AppModel.dataDirectory + "/scrolltest.txt", atomically: true, encoding: .utf8)
        }

        @objc func openSelected() {
            guard let table, table.selectedRow >= 0, let r = row(at: table.selectedRow) else { return }
            if model.showsDrafts {
                model.openDraft(threadID: r.threadId)
            } else {
                model.open(threadID: r.threadId)
            }
        }

        /// In Drafts a click edits the draft; J and K still read through them.
        @objc func clicked() {
            guard model.showsDrafts, let table, table.clickedRow >= 0, table.numberOfSelectedRows == 1 else { return }
            openSelected()
        }

        func move(_ delta: Int) {
            guard let table, count > 0 else { return }
            let next = max(0, min(count - 1, table.selectedRow + delta))
            table.selectRowIndexes([next], byExtendingSelection: false)
            table.scrollRowToVisible(next)
        }

        /// Keys typed in the main window, looked up in the keymap. `g` starts
        /// a two-key sequence that lapses after a second and a half. Returns
        /// false to let AppKit have the key.
        func handle(_ event: NSEvent) -> Bool {
            // ⌘ shortcuts belong to the menus (⌘U is Unsubscribe, not U).
            if event.modifierFlags.contains(.command) { return false }
            // While a panel is up, keys that reach the list close it rather than act on mail.
            if model.findTime != nil {
                if event.keyCode == 53 { model.findTime = nil }
                return true
            }
            if model.palette != nil || model.showsHelp {
                if event.keyCode == 53 { model.palette = nil; model.showsHelp = false }
                return true
            }
            if case .calendars = model.rsvpPanel {
                switch event.keyCode {
                case 125: model.moveAddHighlight(by: 1)
                case 126: model.moveAddHighlight(by: -1)
                case 36, 76: if let message = model.rsvpMessage { model.addToCalendar(message: message, calendar: model.addHighlight) }
                default: model.rsvpPanel = nil
                }
                return true
            }
            // The propose panel's date fields and buttons take every key.
            if case .propose = model.rsvpPanel {
                if event.keyCode == 53 { model.rsvpPanel = nil; return true }
                return false
            }
            if case .scope = model.rsvpPanel {
                switch event.keyCode {
                case 125, 126: model.answerChoices.occurrenceOnly.toggle()
                default: model.rsvpPanel = nil
                }
                return true
            }
            switch event.keyCode {
            case 36, 76: openSelected(); return true
            case 48: model.nextSection(backwards: event.modifierFlags.contains(.shift)); return true
            default: break
            }
            // `characters` keeps what Shift and Option produce, so `#` and `?`
            // work on layouts that type them with Option (Belgian AZERTY).
            guard let typed = event.characters?.lowercased(), !typed.isEmpty else { return false }
            if let started = prefixTime, started.timeIntervalSinceNow < -1.5 { prefix = nil }
            if prefix == nil, typed == "g" {
                prefix = "g"
                prefixTime = .now
                return true
            }
            let after = prefix
            prefix = nil
            guard let command = Command.forKey(typed, after: after) else { return after != nil }
            switch command {
            case .next: move(1)
            case .previous: move(-1)
            case .open: openSelected()
            default: model.run(command)
            }
            return true
        }

        private var prefix: String?
        private var prefixTime: Date?
    }
}

final class KeyTableView: NSTableView {
    override var acceptsFirstResponder: Bool { true }
}

/// Graphite's cursor row: a soft fill and an accent caret. Unread rows get a
/// neutral dot; the accent is reserved for keyboard focus.
final class ThreadRowView: NSTableRowView {
    static let id = NSUserInterfaceItemIdentifier("ThreadRowView")
    var unread = false { didSet { needsDisplay = true } }

    init() {
        super.init(frame: .zero)
        identifier = Self.id
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var isSelected: Bool {
        didSet { updateMark() }
    }

    /// One selected row is the keyboard cursor; several are a selection.
    func updateMark() {
        let several = ((superview as? NSTableView)?.numberOfSelectedRows ?? 0) > 1
        subviews.lazy.compactMap { $0 as? ThreadCell }.first?.mark = isSelected ? (several ? .selected : .cursor) : .none
        needsDisplay = true
    }

    override var isEmphasized: Bool {
        get { false }
        set {}
    }

    override func drawBackground(in dirtyRect: NSRect) {
        guard unread, !isSelected else { return }
        Theme.text.setFill()
        NSBezierPath(ovalIn: NSRect(x: 15, y: bounds.midY - 3, width: 6, height: 6)).fill()
    }

    override func drawSelection(in dirtyRect: NSRect) {
        Theme.fill.setFill()
        NSBezierPath(roundedRect: bounds.insetBy(dx: 8, dy: 0), xRadius: 8, yRadius: 8).fill()
        guard ((superview as? NSTableView)?.numberOfSelectedRows ?? 0) <= 1 else { return }
        Theme.accent.setFill()
        let caret = NSBezierPath()
        let y = bounds.midY
        caret.move(to: NSPoint(x: 9, y: y - 6))
        caret.line(to: NSPoint(x: 15, y: y))
        caret.line(to: NSPoint(x: 9, y: y + 6))
        caret.close()
        caret.fill()
    }
}

final class ThreadCell: NSTableCellView {
    static let id = NSUserInterfaceItemIdentifier("ThreadCell")
    private let sender = NSTextField(labelWithString: "")
    private let subject = NSTextField(labelWithString: "")
    private let date = NSTextField(labelWithString: "")
    private let clip = NSImageView()
    private let invitation = NSImageView()
    private let answer = ChipView()
    /// The paperclip, the calendar mark and the answer, right of the subject.
    private let badges = NSStackView()
    private let keys = NSStackView()
    private var badgesBesideDate: NSLayoutConstraint!
    private var badgesBesideKeys: NSLayoutConstraint!
    private var row: ThreadRow?

    enum Mark { case none, selected, cursor }

    /// The cursor row trades its date for action key hints; rows in a
    /// selection of several keep their dates.
    var mark = Mark.none {
        didSet {
            guard mark != oldValue else { return }
            keys.isHidden = mark != .cursor
            date.isHidden = mark == .cursor
            // The paperclip and the invitation stay, left of the key hints on the cursor row.
            badgesBesideDate.isActive = mark != .cursor
            badgesBesideKeys.isActive = mark == .cursor
        }
    }

    init() {
        super.init(frame: .zero)
        identifier = Self.id
        for f in [sender, subject, date] {
            f.lineBreakMode = .byTruncatingTail
            f.cell?.usesSingleLineMode = true
            f.translatesAutoresizingMaskIntoConstraints = false
            addSubview(f)
        }
        clip.image = NSImage(systemSymbolName: "paperclip", accessibilityDescription: "Has attachments")
        invitation.image = NSImage(systemSymbolName: "calendar", accessibilityDescription: "Invitation")
        for icon in [clip, invitation] {
            icon.symbolConfiguration = .init(pointSize: 11, weight: .regular)
            icon.contentTintColor = Theme.text3
            icon.widthAnchor.constraint(equalToConstant: 14).isActive = true
        }
        badges.orientation = .horizontal
        badges.spacing = 6
        for v in [clip, invitation, answer] as [NSView] { badges.addArrangedSubview(v) }
        keys.orientation = .horizontal
        keys.spacing = 4
        for k in ["E", "H", "R"] { keys.addArrangedSubview(KeyCapView(key: k)) }
        keys.isHidden = true
        for v in [badges, keys] as [NSView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            addSubview(v)
        }
        sender.setContentCompressionResistancePriority(.required, for: .horizontal)
        date.setContentCompressionResistancePriority(.required, for: .horizontal)
        subject.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        date.alignment = .right
        NSLayoutConstraint.activate([
            sender.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 26),
            sender.widthAnchor.constraint(equalToConstant: Theme.senderWidth),
            sender.centerYAnchor.constraint(equalTo: centerYAnchor),
            subject.leadingAnchor.constraint(equalTo: sender.trailingAnchor, constant: 12),
            subject.centerYAnchor.constraint(equalTo: centerYAnchor),
            subject.trailingAnchor.constraint(lessThanOrEqualTo: badges.leadingAnchor, constant: -8),
            badges.centerYAnchor.constraint(equalTo: centerYAnchor),
            date.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -20),
            date.widthAnchor.constraint(equalToConstant: 52),
            date.centerYAnchor.constraint(equalTo: centerYAnchor),
            keys.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -18),
            keys.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
        badgesBesideDate = badges.trailingAnchor.constraint(equalTo: date.leadingAnchor, constant: -10)
        badgesBesideKeys = badges.trailingAnchor.constraint(equalTo: keys.leadingAnchor, constant: -10)
        badgesBesideDate.isActive = true
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    /// The seal in front of a VIP's name. It leads the name so truncation never hides it.
    private static let vipMark = mark("checkmark.seal.fill", label: "VIP")
    /// In front of the subject when AI sorting says the conversation waits for a reply.
    private static let replyMark = mark("arrowshape.turn.up.left.fill", label: "Needs your reply")
    /// In front of the subject of a starred conversation.
    private static let starMark = mark("star.fill", label: "Starred", tint: Theme.star)

    private static func mark(_ symbolName: String, label: String, tint: NSColor = Theme.text2) -> NSTextAttachment {
        let mark = NSTextAttachment()
        let config = NSImage.SymbolConfiguration(pointSize: 10.5, weight: .semibold)
        guard let symbol = NSImage(systemSymbolName: symbolName, accessibilityDescription: label)?
            .withSymbolConfiguration(config) else { return mark }
        // Tinted as it draws, so cut-outs stay cut out and the colour follows the appearance.
        let image = NSImage(size: symbol.size, flipped: false) { rect in
            symbol.draw(in: rect)
            tint.set()
            rect.fill(using: .sourceAtop)
            return true
        }
        image.accessibilityDescription = label
        mark.image = image
        mark.bounds = NSRect(x: 0, y: -1.5, width: symbol.size.width, height: symbol.size.height)
        return mark
    }

    func show(_ r: ThreadRow) {
        row = r
        let unread = r.unreadCount > 0
        let name = NSMutableAttributedString()
        if r.vip {
            name.append(NSAttributedString(attachment: Self.vipMark))
            name.append(NSAttributedString(string: " "))
        }
        name.append(NSAttributedString(string: r.fromName.isEmpty ? r.fromAddr : r.fromName, attributes: [
            .font: Theme.font(weight: unread ? .semibold : .regular),
            .foregroundColor: unread ? Theme.text : Theme.text2,
        ]))
        if r.messageCount > 1 {
            name.append(NSAttributedString(string: "  \(r.messageCount)", attributes: [
                .font: Theme.font(11.5), .foregroundColor: Theme.text3,
            ]))
        }
        sender.attributedStringValue = name
        let line = NSMutableAttributedString()
        if r.flagged {
            line.append(NSAttributedString(attachment: Self.starMark))
            line.append(NSAttributedString(string: " "))
        }
        if r.needsReply {
            line.append(NSAttributedString(attachment: Self.replyMark))
            line.append(NSAttributedString(string: " "))
        }
        line.append(NSAttributedString(string: r.subject.isEmpty ? "(no subject)" : r.subject, attributes: [
            .font: Theme.font(weight: unread ? .semibold : .regular),
            .foregroundColor: unread ? Theme.text : Theme.text2,
        ]))
        if !r.snippet.isEmpty {
            line.append(NSAttributedString(string: "  " + r.snippet, attributes: [
                .font: Theme.font(), .foregroundColor: Theme.text3,
            ]))
        }
        subject.attributedStringValue = line
        date.attributedStringValue = NSAttributedString(string: Theme.shortDate(r.date), attributes: [
            .font: Theme.timeFont, .foregroundColor: Theme.text3,
        ])
        // An invitation's .ics part counts as an attachment; the calendar mark says it better.
        clip.isHidden = !r.hasAttachments || r.invitation != nil
        invitation.isHidden = r.invitation == nil
        answer.text = r.invitation.flatMap(Self.words) ?? ""
        answer.waiting = r.invitation == .unanswered
        answer.isHidden = answer.text.isEmpty
    }

    private static func words(_ state: InvitationState) -> String? {
        switch state {
        case .unanswered: "Not answered"
        case .answered(.accepted): "Accepted"
        case .answered(.tentative): "Maybe"
        case .answered(.declined): "Declined"
        case .answered(.delegated): "Delegated"
        case .answered(.needsAction): "Not answered"
        case .cancelled: "Cancelled"
        case .plain: nil
        }
    }
}

/// Graphite's chip: a short label in a hairline box. One still waiting for
/// the user reads a step stronger than one that is done.
final class ChipView: NSView {
    var text = "" {
        didSet {
            guard text != oldValue else { return }
            invalidateIntrinsicContentSize()
            needsDisplay = true
            setAccessibilityLabel(text)
        }
    }
    var waiting = false { didSet { if waiting != oldValue { needsDisplay = true } } }

    override init(frame: NSRect) {
        super.init(frame: frame)
        translatesAutoresizingMaskIntoConstraints = false
        setContentCompressionResistancePriority(.required, for: .horizontal)
        setAccessibilityElement(true)
        setAccessibilityRole(.staticText)
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    private var label: NSAttributedString {
        NSAttributedString(string: text, attributes: [.font: Theme.font(11), .foregroundColor: waiting ? Theme.text2 : Theme.text3])
    }

    override var intrinsicContentSize: NSSize {
        NSSize(width: ceil(label.size().width) + 12, height: 16)
    }

    override func draw(_ dirtyRect: NSRect) {
        Theme.line.setStroke()
        let box = NSBezierPath(roundedRect: bounds.insetBy(dx: 0.5, dy: 0.5), xRadius: 5, yRadius: 5)
        box.lineWidth = 1
        box.stroke()
        let label = label
        let size = label.size()
        label.draw(at: NSPoint(x: (bounds.width - size.width) / 2, y: (bounds.height - size.height) / 2))
    }
}

/// AppKit twin of `KeyCap`, outlined in the accent because it only shows on the cursor row.
final class KeyCapView: NSView {
    private let key: String

    init(key: String) {
        self.key = key
        super.init(frame: NSRect(x: 0, y: 0, width: 17, height: 17))
        translatesAutoresizingMaskIntoConstraints = false
        widthAnchor.constraint(equalToConstant: 17).isActive = true
        heightAnchor.constraint(equalToConstant: 17).isActive = true
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override func draw(_ dirtyRect: NSRect) {
        Theme.accent.withAlphaComponent(0.55).setStroke()
        let path = NSBezierPath(roundedRect: bounds.insetBy(dx: 0.5, dy: 0.5), xRadius: 5, yRadius: 5)
        path.lineWidth = 1
        path.stroke()
        let text = NSAttributedString(string: key, attributes: [
            .font: NSFont.systemFont(ofSize: 10.5, weight: .medium), .foregroundColor: Theme.accentText,
        ])
        let size = text.size()
        text.draw(at: NSPoint(x: (bounds.width - size.width) / 2, y: (bounds.height - size.height) / 2))
    }
}
