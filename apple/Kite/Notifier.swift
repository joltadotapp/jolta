import Foundation
import UserNotifications

/// Which new mail gets a banner, set in Settings › VIPs.
enum NewMailBanners: String, CaseIterable {
    case all, vips, none

    static var current: NewMailBanners {
        UserDefaults.standard.string(forKey: "NewMailBanners").flatMap(Self.init) ?? .all
    }

    func shows(vip: Bool) -> Bool {
        switch self {
        case .all: true
        case .vips: vip
        case .none: false
        }
    }
}

/// New-mail banners. Clicking one opens its thread.
@MainActor
final class Notifier: NSObject, UNUserNotificationCenterDelegate {
    var onOpen: ((Int64) -> Void)?

    override init() {
        super.init()
        UNUserNotificationCenter.current().delegate = self
    }

    func requestPermission() {
        UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound, .badge]) { _, _ in }
    }

    func post(threadID: Int64, from: String, subject: String) {
        let content = UNMutableNotificationContent()
        content.title = from
        content.body = subject.isEmpty ? "(no subject)" : subject
        content.sound = .default
        content.threadIdentifier = "thread-\(threadID)"
        content.userInfo = ["thread": threadID]
        let request = UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: nil)
        UNUserNotificationCenter.current().add(request)
    }

    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse) async {
        guard let thread = response.notification.request.content.userInfo["thread"] as? Int64 else { return }
        await MainActor.run { onOpen?(thread) }
    }

    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification) async -> UNNotificationPresentationOptions {
        [.banner, .sound]
    }
}
