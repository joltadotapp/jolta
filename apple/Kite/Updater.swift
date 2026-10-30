import Sparkle
import SwiftUI

/// Sparkle checks the feed in Kite-Info.plist once an hour and on demand, and
/// installs a signed update after asking.
@Observable
final class Updater {
    static let shared = Updater()

    private let controller = SPUStandardUpdaterController(startingUpdater: true, updaterDelegate: nil, userDriverDelegate: nil)
    private(set) var canCheck = false
    private var observation: NSKeyValueObservation?

    private init() {
        observation = controller.updater.observe(\.canCheckForUpdates, options: [.initial, .new]) { [weak self] updater, _ in
            let can = updater.canCheckForUpdates
            MainActor.assumeIsolated { self?.canCheck = can }
        }
    }

    func check() { controller.checkForUpdates(nil) }
}

struct CheckForUpdatesButton: View {
    let updater = Updater.shared

    var body: some View {
        Button("Check for Updates…") { updater.check() }
            .disabled(!updater.canCheck)
    }
}
