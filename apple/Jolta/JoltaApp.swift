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
            .frame(minWidth: 1140, minHeight: 600)
            .background(WindowFrameMemory(name: "Jolta main window"))
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
            CommandMenu("Format") {
                Button("Bold") { RichTextView.focused?.toggle(.bold) }.keyboardShortcut("b")
                Button("Italic") { RichTextView.focused?.toggle(.italic) }.keyboardShortcut("i")
                Button("Strikethrough") { RichTextView.focused?.toggle(.strike) }.keyboardShortcut("x", modifiers: [.command, .shift])
                Divider()
                Button("Add Link…") { RichTextView.focused?.addLink() }.keyboardShortcut("k", modifiers: [.command, .shift])
            }
            CommandMenu("Conversation") {
                Button("Unsubscribe") { model?.run(.unsubscribe) }
                    .keyboardShortcut("u")
                    .disabled(model?.compose != nil)
                // While writing, ⌘J belongs to Write with AI in the compose view.
                Button(model?.searchFocused == true ? "Search with AI" : "Summarize") { model?.run(.summarize) }
                    .keyboardShortcut("j")
                    .disabled(model?.compose != nil || (model?.openThread == nil && model?.searchFocused != true))
            }
            CommandMenu("Go") {
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

/// Jolta keeps syncing with its window closed; the menu bar item shows unread
/// mail and brings the window back.
struct MenuBarLabel: View {
    let model: AppModel?

    var body: some View {
        let unread = model.map { _ = $0.listRevision; return $0.unreadCount("in:inbox") } ?? 0
        Image(systemName: unread > 0 ? "envelope.badge" : "envelope")
        if unread > 0 { Text("\(unread)") }
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
        Button("Sync Now") { model?.core.syncNow() }
            .disabled(model == nil)
        CheckForUpdatesButton()
        Divider()
        Button("Quit Jolta") { NSApp.terminate(nil) }
            .keyboardShortcut("q")
    }
}
