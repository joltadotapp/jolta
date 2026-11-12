@preconcurrency import JoltaFFI
import SwiftUI

@main
struct JoltaApp: App {
    @NSApplicationDelegateAdaptor private var delegate: AppDelegate
    @State private var model: AppModel?
    @State private var failure: String?

    var body: some Scene {
        Window("Jolta", id: "main") {
            Group {
                if let model {
                    ContentView(model: model)
                } else if let failure {
                    Text(failure).padding(40)
                } else {
                    ProgressView()
                }
            }
            .frame(minWidth: 640, minHeight: 600)
            .background(WindowFrameMemory(name: "Jolta main window"))
            .onOpenURL { url in model?.subscribe(link: url) }
            .task {
                guard model == nil else { return }
                await ReaderPage.prepare()
                do { model = try AppModel() } catch { failure = "Jolta could not open its data: \(error)" }
            }
        }
        .windowStyle(.hiddenTitleBar)
        .defaultSize(width: 1440, height: 900)
        .restorationBehavior(.disabled)
        .defaultLaunchBehavior(.presented)
        .commands {
            CommandGroup(after: .appInfo) { CheckForUpdatesButton() }
            CommandGroup(replacing: .appSettings) { SettingsMenuButton() }
            CommandGroup(replacing: .newItem) { NewMessageButton(model: model) }
            CommandGroup(before: .sidebar) {
                // Folded away for lack of room, it has nothing to show or hide.
                Button(model?.sidebarCollapsed == false && model?.sidebarFits == true ? "Hide Sidebar" : "Show Sidebar") {
                    withAnimation(Theme.settle) { model?.sidebarCollapsed.toggle() }
                }
                .keyboardShortcut("s", modifiers: [.control, .command])
                .disabled(model?.sidebarFits != true)
            }
            CommandGroup(replacing: .undoRedo) {
                Button("Undo") { model?.undoFromMenu() }
                    .keyboardShortcut("z")
                Button("Redo") { model?.redoFromMenu() }
                    .keyboardShortcut("z", modifiers: [.command, .shift])
            }
            CommandMenu("Format") { FormatMenu() }
            CommandMenu("Message") {
                ForEach(Array(Command.menu.enumerated()), id: \.offset) { i, group in
                    if i > 0 { Divider() }
                    ForEach(group) { command in
                        Button(command.menuTitle) { model?.run(command) }
                            .disabled(model?.offers(command) != true)
                    }
                }
            }
            CommandMenu("Conversation") {
                // While a text editor has the keyboard, ⌘U underlines.
                Button("Unsubscribe") { model?.run(.unsubscribe) }
                    .keyboardShortcut(FormatState.shared.editing ? nil : KeyboardShortcut("u"))
                    .disabled(model?.compose != nil)
                // While writing, ⌘J belongs to Write with AI in the compose view.
                Button(model?.searchFocused == true ? "Search with AI" : "Summarize") { model?.run(.summarize) }
                    .keyboardShortcut("j")
                    .disabled(model?.compose != nil || (model?.openThread == nil && model?.searchFocused != true))
            }
            CommandMenu("Go") {
                Button("Mail") { model?.mode = .mail }
                    .keyboardShortcut("1")
                Button("Calendar") { model?.mode = .calendar }
                    .keyboardShortcut("2")
                Button(model?.showsDayPanel == true ? "Hide Today Beside Mail" : "Show Today Beside Mail") { model?.showsDayPanel.toggle() }
                    .disabled(model?.mode != .mail)
                Button("Join Next Meeting") { model?.joinNextMeeting() }
                    .keyboardShortcut("j", modifiers: [.command, .shift])
                Button("Duplicate Event") { model?.calendar.duplicateSelected() }
                    .keyboardShortcut("d")
                    .disabled(model?.mode != .calendar || model?.calendar.selected == nil || model?.calendar.editor != nil)
                Divider()
                Button("Command Palette") { model?.palette = .commands }
                    .keyboardShortcut("k")
                Button("Keyboard Shortcuts") { model?.showsHelp.toggle() }
                    .keyboardShortcut("/", modifiers: .command)
                Divider()
                Button("Sync Now") { model?.core.syncNow() }
                    .keyboardShortcut("r", modifiers: [.command, .shift])
            }
        }

        Window("Settings", id: "settings") {
            SettingsView(model: model)
                .background(WindowFrameMemory(name: "Jolta settings window"))
        }
        .windowStyle(.hiddenTitleBar)
        .defaultSize(width: 1000, height: 720)
        .restorationBehavior(.disabled)
        .windowResizability(.contentMinSize)

        MenuBarExtra {
            MenuBarContent(model: model)
        } label: {
            MenuBarLabel(model: model)
        }
    }
}

/// ⌘N, the same as C in the list. Off while a message is being written,
/// which it would replace.
struct NewMessageButton: View {
    let model: AppModel?
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Button("New Message") {
            openWindow(id: "main")
            model?.palette = nil
            model?.startCompose()
        }
        .keyboardShortcut("n")
        .disabled(model == nil || model?.compose != nil)
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationDidFinishLaunching(_ notification: Notification) { _ = Updater.shared }
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }
}

/// Jolta keeps syncing with its window closed; the menu bar item shows the
/// Dock badge's number and brings the window back.
struct MenuBarLabel: View {
    let model: AppModel?

    var body: some View {
        let unread = Int(model?.badgeCount ?? 0)
        Image(systemName: unread > 0 ? "envelope.badge" : "envelope")
        if let model, let next = model.calendar.nextMeeting, let text = Self.soon(next, now: model.calendar.now) {
            Text(text)
        } else if unread > 0 {
            Text("\(unread)")
        }
    }

    /// "Standup in 12 min" or "Standup now", within the hour before it starts and while it runs.
    static func soon(_ next: NextMeetingRecord, now: Date) -> String? {
        let t = Int64(now.timeIntervalSince1970)
        guard next.start - t <= 3600, t < next.end else { return nil }
        let title = next.title.count > 24 ? String(next.title.prefix(23)) + "…" : next.title
        return "\(title) \(CalendarModel.countdown(start: next.start, end: next.end, now: now))"
    }
}

struct MenuBarContent: View {
    let model: AppModel?
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Button("Open Jolta") {
            openWindow(id: "main")
            NSApp.activate()
        }
        .keyboardShortcut("o")
        if let model, let next = model.calendar.nextMeeting {
            Divider()
            Text("\(next.title), \(CalendarModel.countdown(start: next.start, end: next.end, now: model.calendar.now))")
            Button("Join \(next.meeting.provider.name)") { CalendarModel.join(next.meeting) }
            Button("Show in Calendar") {
                openWindow(id: "main")
                NSApp.activate()
                model.mode = .calendar
                model.calendar.reveal(eventID: next.eventId, time: .timed(start: next.start, end: next.end))
            }
            Divider()
        }
        Button("Sync Now") { model?.core.syncNow() }
            .disabled(model == nil)
        CheckForUpdatesButton()
        Divider()
        Button("Quit Jolta") { NSApp.terminate(nil) }
            .keyboardShortcut("q")
    }
}
