@preconcurrency import KiteFFI
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
        if UserDefaults.standard.bool(forKey: "KiteScrollTest") {
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
            c.reload(full: false)
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
        var lastRevision = -1
        var wantsFocus = true
        var focusRequest = 0
        private var reportedLaunch = false
        private var count = 0
        private var pages: [Int: [ThreadRow]] = [:]
        private let pageSize = 100

        init(model: AppModel) { self.model = model }

        func reload(full: Bool) {
            guard let table else { return }
            pages.removeAll(keepingCapacity: true)
            let previous = count
            let selected = table.selectedRow
            count = model.count()
            lastQuery = model.query
            if count == 0, model.selectedThreadID != nil { model.closeThread() }
            let rows = count
            DispatchQueue.main.async { [model] in if model.listCount != rows { model.listCount = rows } }
            if !full, previous != count {
                // An action took rows out (or sync added some): keep the
                // cursor at the same position, which is now the next thread.
                table.noteNumberOfRowsChanged()
                table.reloadData()
                if count > 0 {
                    let row = max(0, min(selected, count - 1))
                    table.selectRowIndexes([row], byExtendingSelection: false)
                    table.scrollRowToVisible(row)
                }
                return
            }
            if full {
                table.reloadData()
                table.scrollRowToVisible(0)
                if count > 0 { table.selectRowIndexes([0], byExtendingSelection: false) }
                if !reportedLaunch, count > 0 {
                    reportedLaunch = true
                    DispatchQueue.main.async {
                        AppModel.timing(String(format: "launch to first rows drawn: %.0f ms (%d threads)", AppModel.sinceLaunch, self.count))
                    }
                }
            } else {
                if previous != count { table.noteNumberOfRowsChanged() }
                let visible = table.rows(in: table.visibleRect)
                if visible.length > 0 {
                    table.reloadData(forRowIndexes: IndexSet(integersIn: visible.location..<min(count, visible.location + visible.length)),
                                     columnIndexes: [0])
                }
            }
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
            cell.isCursor = tableView.isRowSelected(row)
            return cell
        }

        func tableView(_ tableView: NSTableView, rowViewForRow row: Int) -> NSTableRowView? {
            let view = (tableView.makeView(withIdentifier: ThreadRowView.id, owner: nil) as? ThreadRowView) ?? ThreadRowView()
            view.unread = (self.row(at: row)?.unreadCount ?? 0) > 0
            return view
        }

        /// The reader follows the cursor, so j and k read through the list.
        func tableViewSelectionDidChange(_ notification: Notification) {
            guard let table, table.selectedRow >= 0, let r = row(at: table.selectedRow) else { return }
            model.cursor = (table.selectedRow, count)
            model.cursorRow = r
            if model.selectedThreadID != r.threadId { model.open(threadID: r.threadId) }
        }

        /// Scrolls through the whole list in screen-sized steps, one step per
        /// run-loop turn like real scrolling, drawing each one, and writes
        /// per-step timings next to the store. Run with `-KiteScrollTest YES`.
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
            model.open(threadID: r.threadId)
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
        didSet { subviews.lazy.compactMap { $0 as? ThreadCell }.first?.isCursor = isSelected }
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
    private let keys = NSStackView()
    private var row: ThreadRow?

    var isCursor = false {
        didSet {
            guard isCursor != oldValue else { return }
            keys.isHidden = !isCursor
            date.isHidden = isCursor
            clip.isHidden = isCursor || !(row?.hasAttachments ?? false)
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
        clip.symbolConfiguration = .init(pointSize: 11, weight: .regular)
        clip.contentTintColor = Theme.text3
        keys.orientation = .horizontal
        keys.spacing = 4
        for k in ["E", "H", "R"] { keys.addArrangedSubview(KeyCapView(key: k)) }
        keys.isHidden = true
        for v in [clip, keys] as [NSView] {
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
            subject.trailingAnchor.constraint(lessThanOrEqualTo: clip.leadingAnchor, constant: -8),
            clip.trailingAnchor.constraint(equalTo: date.leadingAnchor, constant: -10),
            clip.centerYAnchor.constraint(equalTo: centerYAnchor),
            clip.widthAnchor.constraint(equalToConstant: 14),
            date.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -20),
            date.widthAnchor.constraint(equalToConstant: 52),
            date.centerYAnchor.constraint(equalTo: centerYAnchor),
            keys.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -18),
            keys.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    func show(_ r: ThreadRow) {
        row = r
        let unread = r.unreadCount > 0
        let name = NSMutableAttributedString(string: r.fromName.isEmpty ? r.fromAddr : r.fromName, attributes: [
            .font: Theme.font(weight: unread ? .semibold : .regular),
            .foregroundColor: unread ? Theme.text : Theme.text2,
        ])
        if r.messageCount > 1 {
            name.append(NSAttributedString(string: "  \(r.messageCount)", attributes: [
                .font: Theme.font(11.5), .foregroundColor: Theme.text3,
            ]))
        }
        sender.attributedStringValue = name
        let line = NSMutableAttributedString(string: r.subject.isEmpty ? "(no subject)" : r.subject, attributes: [
            .font: Theme.font(weight: unread ? .semibold : .regular),
            .foregroundColor: unread ? Theme.text : Theme.text2,
        ])
        if !r.snippet.isEmpty {
            line.append(NSAttributedString(string: "  " + r.snippet, attributes: [
                .font: Theme.font(), .foregroundColor: Theme.text3,
            ]))
        }
        subject.attributedStringValue = line
        date.attributedStringValue = NSAttributedString(string: Theme.shortDate(r.date), attributes: [
            .font: Theme.timeFont, .foregroundColor: Theme.text3,
        ])
        clip.isHidden = isCursor || !r.hasAttachments
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
