import Sparkle
import SwiftUI

/// Sparkle checks the feed in Jolta-Info.plist once an hour and on demand, and
/// installs a signed update after asking. Debug builds never start it: every
/// release looks newer than a Debug build, and installing one replaces the
/// build under development.
@Observable
final class Updater {
    static let shared = Updater()

    #if DEBUG
    private static let enabled = false
    #else
    private static let enabled = true
    #endif

    private let feed = TestFeed()
    private let controller: SPUStandardUpdaterController
    private(set) var canCheck = false
    private var observation: NSKeyValueObservation?

    private init() {
        TestFeed.remember()
        controller = SPUStandardUpdaterController(startingUpdater: Self.enabled, updaterDelegate: feed, userDriverDelegate: nil)
        observation = controller.updater.observe(\.canCheckForUpdates, options: [.initial, .new]) { [weak self] updater, _ in
            let can = updater.canCheckForUpdates
            MainActor.assumeIsolated { self?.canCheck = can }
        }
    }

    func check() { controller.checkForUpdates(nil) }
}

/// A test build (`scripts/release.sh` without `--public`) carries the private
/// test feed and `JoltaTestBuild`. Its feed is remembered, so a Mac that ran a
/// test build keeps getting them after it installs a public release, whose own
/// feed is the public one. Deleting the `testFeedURL` default undoes this.
private final class TestFeed: NSObject, SPUUpdaterDelegate {
    private static let key = "testFeedURL"

    static func remember() {
        let info = Bundle.main.infoDictionary ?? [:]
        guard info["JoltaTestBuild"] as? String == "YES", let url = info["SUFeedURL"] as? String, !url.isEmpty else { return }
        UserDefaults.standard.set(url, forKey: key)
    }

    func feedURLString(for updater: SPUUpdater) -> String? {
        UserDefaults.standard.string(forKey: Self.key)
    }
}

struct CheckForUpdatesButton: View {
    let updater = Updater.shared

    var body: some View {
        Button("Check for Updates…") { updater.check() }
            .disabled(!updater.canCheck)
    }
}
