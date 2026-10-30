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
        table.allowsEmptySelection = true
        table.usesAutomaticRowHeights = false
        let column = NSTableColumn(identifier: .init("thread"))
        column.resizingMask = .autoresizingMask
        table.addTableColumn(column)
        table.columnAutoresizingStyle = .uniformColumnAutoresizingStyle
        table.dataSource = context.coordinator
        table.delegate = context.coordinator
        table.target = context.coordinator
        table.doubleAction = #selector(Coordinator.openSelected)
        table.onKey = { [weak coordinator = context.coordinator] key in coordinator?.handle(key) ?? false }
        context.coordinator.table = table

        let scroll = NSScrollView()
        scroll.documentView = table
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
        if model.openThread == nil, let table = c.table, table.window?.firstResponder !== table, c.wantsFocus {
            c.wantsFocus = false
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
        private var count = 0
        private var pages: [Int: [ThreadRow]] = [:]
        private let pageSize = 100

        init(model: AppModel) { self.model = model }

        func reload(full: Bool) {
            guard let table else { return }
            pages.removeAll(keepingCapacity: true)
            let previous = count
            count = model.count()
            lastQuery = model.query
            if full {
                table.reloadData()
                table.scrollRowToVisible(0)
                if count > 0 { table.selectRowIndexes([0], byExtendingSelection: false) }
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
            return cell
        }

        func tableView(_ tableView: NSTableView, rowViewForRow row: Int) -> NSTableRowView? {
            (tableView.makeView(withIdentifier: ThreadRowView.id, owner: nil) as? ThreadRowView) ?? ThreadRowView()
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
            if model.openThread != nil, let r = row(at: next) { model.open(threadID: r.threadId) }
        }

        /// Single-key shortcuts for the list. Returns false to let AppKit handle the key.
        func handle(_ event: NSEvent) -> Bool {
            switch (event.charactersIgnoringModifiers, event.keyCode) {
            case ("j", _): move(1)
            case ("k", _): move(-1)
            case (_, 36), (_, 76), ("o", _): openSelected()
            case (_, 53): model.closeThread()
            default: return false
            }
            return true
        }
    }
}

final class KeyTableView: NSTableView {
    var onKey: ((NSEvent) -> Bool)?

    override func keyDown(with event: NSEvent) {
        if event.modifierFlags.intersection([.command, .control, .option]).isEmpty, onKey?(event) == true { return }
        super.keyDown(with: event)
    }
}

final class ThreadRowView: NSTableRowView {
    static let id = NSUserInterfaceItemIdentifier("ThreadRowView")

    init() {
        super.init(frame: .zero)
        identifier = Self.id
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override func drawSelection(in dirtyRect: NSRect) {
        Theme.selection.setFill()
        NSBezierPath(roundedRect: bounds.insetBy(dx: 6, dy: 1), xRadius: 6, yRadius: 6).fill()
    }
}

final class ThreadCell: NSTableCellView {
    static let id = NSUserInterfaceItemIdentifier("ThreadCell")
    private let sender = NSTextField(labelWithString: "")
    private let subject = NSTextField(labelWithString: "")
    private let date = NSTextField(labelWithString: "")
    private let dot = NSView()

    init() {
        super.init(frame: .zero)
        identifier = Self.id
        for f in [sender, subject, date] {
            f.lineBreakMode = .byTruncatingTail
            f.cell?.usesSingleLineMode = true
            f.translatesAutoresizingMaskIntoConstraints = false
            addSubview(f)
        }
        dot.wantsLayer = true
        dot.layer?.cornerRadius = 3
        dot.layer?.backgroundColor = Theme.accent.cgColor
        dot.translatesAutoresizingMaskIntoConstraints = false
        addSubview(dot)
        sender.setContentCompressionResistancePriority(.required, for: .horizontal)
        date.setContentCompressionResistancePriority(.required, for: .horizontal)
        date.alignment = .right
        NSLayoutConstraint.activate([
            dot.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 14),
            dot.centerYAnchor.constraint(equalTo: centerYAnchor),
            dot.widthAnchor.constraint(equalToConstant: 6),
            dot.heightAnchor.constraint(equalToConstant: 6),
            sender.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 28),
            sender.widthAnchor.constraint(equalToConstant: Theme.senderWidth),
            sender.centerYAnchor.constraint(equalTo: centerYAnchor),
            subject.leadingAnchor.constraint(equalTo: sender.trailingAnchor, constant: 12),
            subject.centerYAnchor.constraint(equalTo: centerYAnchor),
            subject.trailingAnchor.constraint(lessThanOrEqualTo: date.leadingAnchor, constant: -12),
            date.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -16),
            date.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    func show(_ r: ThreadRow) {
        let unread = r.unreadCount > 0
        dot.isHidden = !unread
        let name = r.fromName.isEmpty ? r.fromAddr : r.fromName
        sender.attributedStringValue = NSAttributedString(string: r.messageCount > 1 ? "\(name)  \(r.messageCount)" : name, attributes: [
            .font: unread ? Theme.font(weight: .semibold) : Theme.font(weight: .regular),
            .foregroundColor: unread ? Theme.text : Theme.secondaryText,
        ])
        let line = NSMutableAttributedString(string: r.subject.isEmpty ? "(no subject)" : r.subject, attributes: [
            .font: unread ? Theme.font(weight: .semibold) : Theme.font(weight: .regular),
            .foregroundColor: Theme.text,
        ])
        if !r.snippet.isEmpty {
            line.append(NSAttributedString(string: "  " + r.snippet, attributes: [
                .font: Theme.font(weight: .regular), .foregroundColor: Theme.tertiaryText,
            ]))
        }
        subject.attributedStringValue = line
        date.attributedStringValue = NSAttributedString(string: Theme.shortDate(r.date), attributes: [
            .font: Theme.monoFont, .foregroundColor: Theme.tertiaryText,
        ])
    }
}
