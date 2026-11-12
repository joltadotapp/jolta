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
        model.togglePicked = { [weak coordinator = context.coordinator] in coordinator?.togglePick() }
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
        if let table = c.table, c.focusRequest != model.focusListRequest {
            c.focusRequest = model.focusListRequest
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
        var focusRequest = 0
        private var reportedLaunch = false
        /// The conversation under the caret while X picks (`KeyTableView.pickCursor`),
        /// to find its row again when the list changes.
        private var pickThread: Int64?
        /// X is changing the selection, so picking goes on.
        private var changingPick = false
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
                setPick(nil)
                table.reloadData()
                // A thread opened before the list was built, such as from a
                // notification while calendar mode showed, keeps its row. A
                // list the user switched to opens at its top; at launch, the
                // same list shown again or a search typed, nothing else opens,
                // as opening marks it read. A result opens when the user
                // moves to it.
                let switched = (model.shownListQuery.map { $0 != model.listQuery } ?? false) && model.search.isEmpty
                model.shownListQuery = model.listQuery
                let opened = model.selectedThreadID.flatMap { model.rows(of: [$0]).first }
                let row = opened ?? 0
                table.scrollRowToVisible(row)
                if count > 0, opened != nil || switched {
                    table.selectRowIndexes([row], byExtendingSelection: false)
                }
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
                // Reloading cells leaves row views alone, and they draw the unread dot and the VIP seal.
                table.enumerateAvailableRowViews { view, row in
                    (view as? ThreadRowView)?.lead = self.lead(at: row)
                }
            }
            guard count > 0 else { return setPick(nil) }
            var kept = model.rows(of: model.selectedThreads)
            // The caret follows its conversation while the picked ones are still there.
            var caret = pickThread.flatMap { model.rows(of: [$0]).first }
            if kept.isEmpty {
                guard userChanged else {
                    table.deselectAll(nil)
                    if model.selectedThreadID != nil || !model.selectedThreads.isEmpty { model.closeThread() }
                    return
                }
                kept = [min(anchor, count - 1)]
                caret = nil
            }
            if kept != table.selectedRowIndexes {
                changingPick = true
                table.selectRowIndexes(kept, byExtendingSelection: false)
                changingPick = false
            }
            setPick(caret)
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

        private func lead(at index: Int) -> ThreadRowView.Lead {
            guard let r = row(at: index) else { return .none }
            return r.vip ? .vip : r.unreadCount > 0 ? .unread : .none
        }

        func numberOfRows(in tableView: NSTableView) -> Int { count }

        func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
            let cell = (tableView.makeView(withIdentifier: ThreadCell.id, owner: nil) as? ThreadCell) ?? ThreadCell()
            if let r = self.row(at: row) { cell.show(r) }
            cell.mark = (tableView as? KeyTableView)?.mark(row: row) ?? .none
            return cell
        }

        func tableView(_ tableView: NSTableView, rowViewForRow row: Int) -> NSTableRowView? {
            let view = (tableView.makeView(withIdentifier: ThreadRowView.id, owner: nil) as? ThreadRowView) ?? ThreadRowView()
            view.lead = lead(at: row)
            view.hovering = row == (tableView as? KeyTableView)?.hoverRow
            return view
        }

        /// The reader follows the cursor, so j and k read through the list.
        /// Shift-click and Command-click select several threads instead.
        func tableViewSelectionDidChange(_ notification: Notification) {
            // A click, an arrow key or anything else that selects ends picking.
            if !changingPick { setPick(nil) }
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
            if let caret = table.pickCursor {
                let next = max(0, min(count - 1, caret + delta))
                setPick(next)
                return table.scrollRowToVisible(next)
            }
            let next = max(0, min(count - 1, table.selectedRow + delta))
            table.selectRowIndexes([next], byExtendingSelection: false)
            table.scrollRowToVisible(next)
        }

        private func setPick(_ row: Int?) {
            pickThread = row.flatMap { self.row(at: $0)?.threadId }
            table?.pickCursor = row
        }

        /// X: adds the conversation under the caret to the selection, or
        /// takes it off. From then on J and K move the caret and leave the
        /// selection alone, until Esc or a key or click that selects.
        func togglePick() {
            guard let table else { return }
            guard let caret = table.pickCursor else {
                guard table.selectedRow >= 0 else { return }
                setPick(table.selectedRow)
                // The one selected row counts as picked. Among several, X takes the last one off.
                if table.numberOfSelectedRows > 1 { togglePick() }
                return
            }
            var rows = table.selectedRowIndexes
            if rows.contains(caret) { rows.remove(caret) } else { rows.insert(caret) }
            // With nothing picked any more, the caret's row is the cursor again.
            guard !rows.isEmpty else { return setPick(nil) }
            changingPick = true
            table.selectRowIndexes(rows, byExtendingSelection: false)
            changingPick = false
        }

        /// ⇧J and ⇧K: the table extends the selection itself, as it does for ⇧↓ and ⇧↑.
        private func extend(_ delta: Int, like event: NSEvent) {
            guard let table, count > 0 else { return }
            takeKeyboard()
            let arrow = String(UnicodeScalar(delta > 0 ? NSDownArrowFunctionKey : NSUpArrowFunctionKey).map(Character.init) ?? " ")
            guard let key = NSEvent.keyEvent(with: .keyDown, location: event.locationInWindow, modifierFlags: [.shift, .numericPad, .function],
                                             timestamp: event.timestamp, windowNumber: event.windowNumber, context: nil,
                                             characters: arrow, charactersIgnoringModifiers: arrow, isARepeat: event.isARepeat,
                                             keyCode: delta > 0 ? 125 : 126) else { return }
            table.keyDown(with: key)
        }

        /// Esc: a selection of several, or X's picking, goes back to the one
        /// row under the caret. False when there was nothing to clear.
        private func clearSelection() -> Bool {
            guard let table, table.pickCursor != nil || table.numberOfSelectedRows > 1 else { return false }
            let row = table.pickCursor ?? table.selectedRow
            setPick(nil)
            table.selectRowIndexes([row], byExtendingSelection: false)
            return true
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
            case 36, 76:
                takeKeyboard()
                // With nothing open yet, Return opens the top, as j does.
                if (table?.selectedRow ?? 0) < 0 { move(1) } else { openSelected() }
                return true
            case 48:
                // Without sections Tab moves the keyboard on, as anywhere in macOS.
                guard model.showsSections else { return false }
                model.nextSection(backwards: event.modifierFlags.contains(.shift))
                return true
            case 53: return clearSelection()
            default: break
            }
            // `characters` keeps what Shift and Option produce, so `#` and `?`
            // work on layouts that type them with Option (Belgian AZERTY).
            guard let typed = event.characters?.lowercased(), !typed.isEmpty else { return false }
            if event.modifierFlags.contains(.shift), typed == "j" || typed == "k" {
                prefix = nil
                extend(typed == "j" ? 1 : -1, like: event)
                return true
            }
            if let started = prefixTime, started.timeIntervalSinceNow < -1.5 { prefix = nil }
            if prefix == nil, typed == "g" {
                prefix = "g"
                prefixTime = .now
                return true
            }
            let after = prefix
            prefix = nil
            guard let command = Command.forKey(typed, after: after) else { return after != nil }
            takeKeyboard()
            switch command {
            case .next: move(1)
            case .previous: move(-1)
            case .open: openSelected()
            case .select: togglePick()
            default: model.run(command)
            }
            return true
        }

        /// A list key typed while something else has the keyboard, such as
        /// the message after a click in it, gives the keyboard to the list:
        /// the caret then shows where the key landed.
        private func takeKeyboard() {
            guard let table, let window = table.window, window.firstResponder !== table else { return }
            window.makeFirstResponder(table)
        }

        private var prefix: String?
        private var prefixTime: Date?
    }
}

final class KeyTableView: NSTableView {
    override var acceptsFirstResponder: Bool { true }

    private var isFirstResponder = false
    private var keyObservers: [NSObjectProtocol] = []

    /// The list is where typed keys go: it is the first responder of the
    /// key window. Only then does the cursor row show its caret and keys.
    private(set) var hasKeyboard = false {
        didSet { if hasKeyboard != oldValue { updateMarks() } }
    }

    /// The caret's row while X picks conversations one by one: J and K then
    /// move the caret and leave the selection alone. Nil otherwise, when
    /// the one selected row is the cursor.
    var pickCursor: Int? {
        didSet { if pickCursor != oldValue { updateMarks() } }
    }

    private func updateMarks() {
        enumerateAvailableRowViews { view, _ in (view as? ThreadRowView)?.updateMark() }
    }

    /// The row under the pointer, -1 for none. Kept here, not by each row:
    /// a row that scrolls from under a still pointer gets no mouse-exited.
    private(set) var hoverRow = -1 {
        didSet {
            guard hoverRow != oldValue else { return }
            for row in [oldValue, hoverRow] where row >= 0 {
                (rowView(atRow: row, makeIfNecessary: false) as? ThreadRowView)?.hovering = row == hoverRow
            }
        }
    }
    private var hoverArea: NSTrackingArea?
    private var scrollObserver: NSObjectProtocol?

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let hoverArea { removeTrackingArea(hoverArea) }
        let area = NSTrackingArea(rect: .zero, options: [.mouseMoved, .mouseEnteredAndExited, .activeInKeyWindow, .inVisibleRect], owner: self)
        addTrackingArea(area)
        hoverArea = area
    }

    override func mouseEntered(with event: NSEvent) { hover(at: event.locationInWindow) }
    override func mouseMoved(with event: NSEvent) { hover(at: event.locationInWindow) }
    override func mouseExited(with event: NSEvent) { hoverRow = -1 }

    /// A tracking area also reports the pointer over what lies on top of
    /// the list (the palette, a toast), so the row must be what it hits.
    private func hover(at point: NSPoint) {
        let here = convert(point, from: nil)
        let onList = visibleRect.contains(here) && window?.contentView?.hitTest(point)?.isDescendant(of: self) == true
        hoverRow = onList ? row(at: here) : -1
    }

    /// Rows moved under the pointer: scrolling sends no mouse-moved.
    private func hoverAfterScroll() {
        guard let window, window.isKeyWindow else { return }
        hover(at: window.mouseLocationOutsideOfEventStream)
    }

    /// One selected row is the keyboard cursor while the list has the
    /// keyboard; several, one without the keyboard, or rows picked with X
    /// are a selection.
    func mark(row: Int) -> ThreadCell.Mark {
        guard isRowSelected(row) else { return .none }
        return pickCursor == nil && numberOfSelectedRows == 1 && hasKeyboard ? .cursor : .selected
    }

    /// The list has the keyboard as soon as it is in the window, not at
    /// SwiftUI's next update, which may come after the user clicked Search.
    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        keyObservers.forEach(NotificationCenter.default.removeObserver)
        keyObservers = []
        if let scrollObserver { NotificationCenter.default.removeObserver(scrollObserver) }
        scrollObserver = nil
        guard let window else { return hasKeyboard = false }
        if let clip = enclosingScrollView?.contentView {
            clip.postsBoundsChangedNotifications = true
            scrollObserver = NotificationCenter.default.addObserver(forName: NSView.boundsDidChangeNotification, object: clip, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated { self?.hoverAfterScroll() }
            }
        }
        for name in [NSWindow.didBecomeKeyNotification, NSWindow.didResignKeyNotification] {
            keyObservers.append(NotificationCenter.default.addObserver(forName: name, object: window, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated { self?.keyboardChanged() }
            })
        }
        window.makeFirstResponder(self)
    }

    override func becomeFirstResponder() -> Bool {
        let became = super.becomeFirstResponder()
        if became {
            isFirstResponder = true
            keyboardChanged()
        }
        return became
    }

    override func resignFirstResponder() -> Bool {
        let resigned = super.resignFirstResponder()
        if resigned {
            isFirstResponder = false
            keyboardChanged()
        }
        return resigned
    }

    private func keyboardChanged() {
        hasKeyboard = isFirstResponder && window?.isKeyWindow == true
        if window?.isKeyWindow != true { hoverRow = -1 }
    }
}

/// Graphite's cursor row: a soft fill and an accent caret. Unread rows get a
/// neutral dot; the accent is reserved for keyboard focus. The caret, the
/// dot and a VIP's seal share one slot in front of the sender, so every
/// sender starts at the same place.
final class ThreadRowView: NSTableRowView {
    static let id = NSUserInterfaceItemIdentifier("ThreadRowView")

    /// What stands in front of the sender. A VIP's unread mail shows the
    /// seal; its bold sender and subject say it is unread.
    enum Lead { case none, unread, vip }

    var lead = Lead.none { didSet { if lead != oldValue { needsDisplay = true } } }
    var hovering = false { didSet { if hovering != oldValue { needsDisplay = true } } }

    init() {
        super.init(frame: .zero)
        identifier = Self.id
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override var isSelected: Bool {
        didSet { updateMark() }
    }

    private var table: KeyTableView? { superview as? KeyTableView }

    /// The caret is on this row: the one selected row, or the row X acts on.
    private var showsCaret: Bool {
        guard let table, table.hasKeyboard else { return false }
        if let caret = table.pickCursor { return table.row(for: self) == caret }
        return isSelected && table.numberOfSelectedRows == 1
    }

    func updateMark() {
        subviews.lazy.compactMap { $0 as? ThreadCell }.first?.mark = !isSelected ? .none
            : showsCaret && table?.pickCursor == nil ? .cursor : .selected
        needsDisplay = true
    }

    override var isEmphasized: Bool {
        get { false }
        set {}
    }

    override func drawBackground(in dirtyRect: NSRect) {
        guard !isSelected else { return }
        if hovering {
            Theme.hover.setFill()
            NSBezierPath(roundedRect: bounds.insetBy(dx: 8, dy: 0), xRadius: Theme.radiusRow, yRadius: Theme.radiusRow).fill()
        }
        // While X picks, the caret also stands on rows that aren't picked.
        drawSlot()
    }

    override func drawSelection(in dirtyRect: NSRect) {
        Theme.fill.setFill()
        NSBezierPath(roundedRect: bounds.insetBy(dx: 8, dy: 0), xRadius: Theme.radiusRow, yRadius: Theme.radiusRow).fill()
        drawSlot()
    }

    /// The caret where the keyboard is, else the seal or the dot.
    private func drawSlot() {
        if showsCaret { return drawCaret() }
        let middle = NSPoint(x: ThreadCell.senderLeading - 8, y: bounds.midY)
        switch lead {
        case .none: break
        case .unread:
            Theme.text.setFill()
            NSBezierPath(ovalIn: NSRect(x: middle.x - 3, y: middle.y - 3, width: 6, height: 6)).fill()
        case .vip:
            guard let seal = ThreadCell.vipSeal else { return }
            let size = seal.size
            seal.draw(in: NSRect(x: (middle.x - size.width / 2).rounded(), y: (middle.y - size.height / 2).rounded(), width: size.width, height: size.height),
                      from: .zero, operation: .sourceOver, fraction: 1, respectFlipped: true, hints: nil)
        }
    }

    private func drawCaret() {
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
    private var row: ThreadRow?

    /// Where every sender starts; the row view's slot lies in front of it.
    static let senderLeading: CGFloat = 26
    /// The date and the cursor row's three key caps share this width and
    /// the same right edge, so the badges beside them stay put as the cursor moves.
    private static let trailingWidth: CGFloat = 3 * 17 + 2 * 4

    enum Mark { case none, selected, cursor }

    /// The cursor row trades its date for action key hints; rows in a
    /// selection, and the selected row while the list doesn't have the
    /// keyboard, keep their dates.
    var mark = Mark.none {
        didSet {
            guard mark != oldValue else { return }
            keys.isHidden = mark != .cursor
            date.isHidden = mark == .cursor
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
            sender.leadingAnchor.constraint(equalTo: leadingAnchor, constant: Self.senderLeading),
            sender.widthAnchor.constraint(equalToConstant: Theme.senderWidth),
            sender.centerYAnchor.constraint(equalTo: centerYAnchor),
            subject.leadingAnchor.constraint(equalTo: sender.trailingAnchor, constant: 12),
            subject.centerYAnchor.constraint(equalTo: centerYAnchor),
            subject.trailingAnchor.constraint(lessThanOrEqualTo: badges.leadingAnchor, constant: -8),
            badges.centerYAnchor.constraint(equalTo: centerYAnchor),
            badges.trailingAnchor.constraint(equalTo: date.leadingAnchor, constant: -10),
            date.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -20),
            date.widthAnchor.constraint(equalToConstant: Self.trailingWidth),
            date.centerYAnchor.constraint(equalTo: centerYAnchor),
            keys.trailingAnchor.constraint(equalTo: date.trailingAnchor),
            keys.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    /// A VIP's seal, which the row view draws in the slot in front of the sender.
    static let vipSeal = symbol("checkmark.seal.fill", label: "VIP")
    /// In front of the subject when AI sorting says the conversation waits for a reply.
    private static let replyMark = mark("arrowshape.turn.up.left.fill", label: "Needs your reply")
    /// In front of the subject of a starred conversation.
    private static let starMark = mark("star.fill", label: "Starred", tint: Theme.star)

    /// One line that ends in an ellipsis. A label shows an attributed string
    /// by the string's own paragraph style; without one it clips mid-letter.
    private static let oneLine: NSParagraphStyle = {
        let style = NSMutableParagraphStyle()
        style.lineBreakMode = .byTruncatingTail
        return style
    }()

    private static func symbol(_ symbolName: String, label: String, tint: NSColor = Theme.text2) -> NSImage? {
        let config = NSImage.SymbolConfiguration(pointSize: 10.5, weight: .semibold)
        guard let symbol = NSImage(systemSymbolName: symbolName, accessibilityDescription: label)?
            .withSymbolConfiguration(config) else { return nil }
        // Tinted as it draws, so cut-outs stay cut out and the colour follows the appearance.
        let image = NSImage(size: symbol.size, flipped: false) { rect in
            symbol.draw(in: rect)
            tint.set()
            rect.fill(using: .sourceAtop)
            return true
        }
        image.accessibilityDescription = label
        return image
    }

    private static func mark(_ symbolName: String, label: String, tint: NSColor = Theme.text2) -> NSTextAttachment {
        let mark = NSTextAttachment()
        guard let image = symbol(symbolName, label: label, tint: tint) else { return mark }
        mark.image = image
        mark.bounds = NSRect(x: 0, y: -1.5, width: image.size.width, height: image.size.height)
        return mark
    }

    func show(_ r: ThreadRow) {
        row = r
        let unread = r.unreadCount > 0
        let name = NSMutableAttributedString()
        name.append(NSAttributedString(string: r.fromName.isEmpty ? r.fromAddr : r.fromName, attributes: [
            .font: Theme.font(weight: unread ? .semibold : .regular),
            .foregroundColor: unread ? Theme.text : Theme.text2,
        ]))
        if r.messageCount > 1 {
            name.append(NSAttributedString(string: "  \(r.messageCount)", attributes: [
                .font: Theme.font(11.5), .foregroundColor: Theme.text3,
            ]))
        }
        name.addAttribute(.paragraphStyle, value: Self.oneLine, range: NSRange(location: 0, length: name.length))
        sender.attributedStringValue = name
        // The seal is drawn beside the name, so VoiceOver hears it here.
        sender.setAccessibilityHelp(r.vip ? "VIP" : nil)
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
        line.addAttribute(.paragraphStyle, value: Self.oneLine, range: NSRange(location: 0, length: line.length))
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
        let box = NSBezierPath(roundedRect: bounds.insetBy(dx: 0.5, dy: 0.5), xRadius: Theme.radiusKey, yRadius: Theme.radiusKey)
        box.lineWidth = 1
        box.stroke()
        let label = label
        let size = label.size()
        label.draw(at: NSPoint(x: (bounds.width - size.width) / 2, y: (bounds.height - size.height) / 2))
    }
}

/// AppKit twin of `KeyCap`'s focused style: it only shows on the cursor row.
final class KeyCapView: NSView {
    private let key: String
    private static let font = NSFont.systemFont(ofSize: 10.5, weight: .medium)

    init(key: String) {
        self.key = key
        // As `KeyCap`: 4 points beside the letter, 17 at least.
        let width = max(17, ceil(NSAttributedString(string: key, attributes: [.font: Self.font]).size().width) + 8)
        super.init(frame: NSRect(x: 0, y: 0, width: width, height: 17))
        translatesAutoresizingMaskIntoConstraints = false
        widthAnchor.constraint(equalToConstant: width).isActive = true
        heightAnchor.constraint(equalToConstant: 17).isActive = true
    }

    required init?(coder: NSCoder) { fatalError("not used") }

    override func draw(_ dirtyRect: NSRect) {
        Theme.accent.setStroke()
        let path = NSBezierPath(roundedRect: bounds.insetBy(dx: 0.5, dy: 0.5), xRadius: Theme.radiusKey, yRadius: Theme.radiusKey)
        path.lineWidth = 1
        path.stroke()
        let text = NSAttributedString(string: key, attributes: [.font: Self.font, .foregroundColor: Theme.accentText])
        let size = text.size()
        text.draw(at: NSPoint(x: (bounds.width - size.width) / 2, y: (bounds.height - size.height) / 2))
    }
}
