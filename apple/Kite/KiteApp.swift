import SwiftUI

@main
struct KiteApp: App {
    @NSApplicationDelegateAdaptor private var delegate: AppDelegate
    @State private var model: AppModel?
    @State private var failure: String?

    var body: some Scene {
        Window("Kite", id: "main") {
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
            .task {
                guard model == nil else { return }
                await ReaderPage.prepare()
                do { model = try AppModel() } catch { failure = "Kite could not open its data: \(error)" }
            }
        }
        .windowStyle(.hiddenTitleBar)
        .defaultSize(width: 1440, height: 900)
        .commands {
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

        Settings {
            SettingsView(model: model)
        }

        MenuBarExtra {
            MenuBarContent(model: model)
        } label: {
            MenuBarLabel(model: model)
        }
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }
}

/// Kite keeps syncing with its window closed; the menu bar item shows unread
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
        Button("Open Kite") {
            openWindow(id: "main")
            NSApp.activate()
        }
        .keyboardShortcut("o")
        Button("Sync Now") { model?.core.syncNow() }
            .disabled(model == nil)
        Divider()
        Button("Quit Kite") { NSApp.terminate(nil) }
            .keyboardShortcut("q")
    }
}
