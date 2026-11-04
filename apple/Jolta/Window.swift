import AppKit
import SwiftUI

/// Single-key commands work wherever focus is in the main window (list,
/// reader, sidebar) and stay out of the way of text: a search field, the
/// palette or a message being written gets every key.
@MainActor
final class KeyRouter {
    static let shared = KeyRouter()
    weak var window: NSWindow?
    /// The message list's key handler; returns true when it used the key.
    var handler: ((NSEvent) -> Bool)?
    /// Calendar mode's keys, and ⇧C for the day panel in mail; asked first.
    var calendarHandler: ((NSEvent) -> Bool)?
    private var monitor: Any?

    func install() {
        guard monitor == nil else { return }
        monitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { event in
            MainActor.assumeIsolated { KeyRouter.shared.route(event) } ? nil : event
        }
    }

    private func route(_ event: NSEvent) -> Bool {
        guard let window, event.window === window, window.attachedSheet == nil else { return false }
        guard event.modifierFlags.intersection([.command, .control]).isEmpty else { return false }
        if let text = window.firstResponder as? NSTextView, text.isEditable { return false }
        if calendarHandler?(event) == true { return true }
        return handler?(event) ?? false
    }
}

/// Tells the key router which window is the main one.
struct MainWindowReader: NSViewRepresentable {
    func makeNSView(context: Context) -> NSView { Reader() }
    func updateNSView(_ view: NSView, context: Context) {}

    private final class Reader: NSView {
        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            if let window {
                KeyRouter.shared.window = window
                KeyRouter.shared.install()
            }
        }
    }
}

/// The window has no title bar of its own; the column headers stand in for
/// it. Placed behind a header, this drags the window and answers a double
/// click the way System Settings says (zoom, minimize or nothing).
struct TitleBarArea: NSViewRepresentable {
    func makeNSView(context: Context) -> NSView { Area() }
    func updateNSView(_ view: NSView, context: Context) {}

    private final class Area: NSView {
        override var mouseDownCanMoveWindow: Bool { true }

        override func mouseDown(with event: NSEvent) {
            guard let window else { return }
            if event.clickCount == 2 {
                switch UserDefaults.standard.string(forKey: "AppleActionOnDoubleClick") {
                case "Minimize": window.performMiniaturize(nil)
                case "None": break
                default: window.performZoom(nil)
                }
            } else {
                window.performDrag(with: event)
            }
        }
    }
}

/// Remembers a window's size and place between launches. Jolta's windows opt
/// out of state restoration, because for it AppKit keeps a picture of the
/// window in memory (20 MB or more on a large screen, two at once while it
/// saves); this keeps the part of restoration people notice.
struct WindowFrameMemory: NSViewRepresentable {
    let name: String

    func makeNSView(context: Context) -> NSView { Holder(name: name) }
    func updateNSView(_ view: NSView, context: Context) {}

    private final class Holder: NSView {
        let name: String

        init(name: String) {
            self.name = name
            super.init(frame: .zero)
        }

        required init?(coder: NSCoder) { nil }

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            guard let window, window.frameAutosaveName != name else { return }
            window.setFrameUsingName(name)
            window.setFrameAutosaveName(name)
        }
    }
}
