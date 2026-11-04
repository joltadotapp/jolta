@preconcurrency import JoltaFFI
import Foundation
import UserNotifications

/// Which new mail gets a banner, set in Settings › People.
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

/// New-mail banners and calendar reminders. Clicking a mail banner opens its
/// thread; a reminder offers Join (online meetings), Snooze 5 minutes and Open.
@MainActor
final class Notifier: NSObject, UNUserNotificationCenterDelegate {
    var onOpen: ((Int64) -> Void)?
    /// An occurrence to show in calendar mode, by event id and time.
    /// A reminder's id and fire time; the app looks the event up again.
    var onOpenEvent: ((String, Int64) -> Void)?
    var onJoin: ((String, Int64) -> Void)?

    /// Scheduled reminders' identifiers start with this; the record's id follows.
    static let reminderPrefix = "cal-"
    private nonisolated static let snoozePrefix = "calsnooze-"
    /// macOS 27 keeps 100 pending notifications per app, and one added past
    /// that pushes out another, not necessarily the latest (measured
    /// 2026-11-04); ten stay free for snoozes.
    static let reminderLimit = 90

    private enum Category {
        static let meeting = "calendar-meeting"
        static let event = "calendar-event"
    }

    private enum Action {
        static let join = "join"
        static let snooze = "snooze"
        static let open = "open"
    }

    override init() {
        super.init()
        let center = UNUserNotificationCenter.current()
        center.delegate = self
        let join = UNNotificationAction(identifier: Action.join, title: "Join", options: [.foreground])
        let snooze = UNNotificationAction(identifier: Action.snooze, title: "Snooze 5 Minutes", options: [])
        let open = UNNotificationAction(identifier: Action.open, title: "Open", options: [.foreground])
        center.setNotificationCategories([
            UNNotificationCategory(identifier: Category.meeting, actions: [join, snooze, open], intentIdentifiers: []),
            UNNotificationCategory(identifier: Category.event, actions: [snooze, open], intentIdentifiers: []),
        ])
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

    /// Makes the pending reminders match `reminders`: each is added under its
    /// stable id, which replaces a pending one with the same id, and pending
    /// reminders no longer listed are removed. Calendar triggers fire with
    /// Jolta closed.
    /// `stillDue` says whether a snoozed reminder still stands; those that
    /// don't are removed, and the rest count against the limit.
    func schedule(reminders: [ReminderRecord], stillDue: (String, Int64) -> Bool) async {
        let center = UNUserNotificationCenter.current()
        let now = Int64(Date.now.timeIntervalSince1970)
        let snoozed = await center.pendingNotificationRequests().filter { $0.identifier.hasPrefix(Self.snoozePrefix) }
        let lapsed = snoozed.filter { request in
            guard let (id, fireAt) = Self.reminder(request.content.userInfo) else { return true }
            return !stillDue(id, fireAt)
        }
        center.removePendingNotificationRequests(withIdentifiers: lapsed.map(\.identifier))
        let wanted = reminders.filter { $0.fireAt > now }.prefix(max(0, Self.reminderLimit - (snoozed.count - lapsed.count)))
        if !wanted.isEmpty {
            _ = try? await center.requestAuthorization(options: [.alert, .sound, .badge])
        }
        let ids = Set(wanted.map { Self.reminderPrefix + $0.id })
        let stale = await center.pendingNotificationRequests().map(\.identifier).filter { $0.hasPrefix(Self.reminderPrefix) && !ids.contains($0) }
        center.removePendingNotificationRequests(withIdentifiers: stale)
        for r in wanted {
            let content = UNMutableNotificationContent()
            content.title = r.title.isEmpty ? "(no title)" : r.title
            content.body = Self.body(r)
            content.sound = .default
            content.threadIdentifier = "calendar"
            content.categoryIdentifier = r.joinUrl == nil ? Category.event : Category.meeting
            content.userInfo = ["reminder": r.id, "fireAt": r.fireAt]
            let when = Calendar.current.dateComponents([.year, .month, .day, .hour, .minute, .second],
                                                       from: Date(timeIntervalSince1970: TimeInterval(r.fireAt)))
            let request = UNNotificationRequest(identifier: Self.reminderPrefix + r.id, content: content,
                                                trigger: UNCalendarNotificationTrigger(dateMatching: when, repeats: false))
            try? await center.add(request)
        }
    }

    /// "10:00–11:00 · Microsoft Teams", or "All day".
    private static func body(_ r: ReminderRecord) -> String {
        var parts: [String] = []
        switch r.time {
        case .timed(let start, let end):
            let s = Date(timeIntervalSince1970: TimeInterval(start)), e = Date(timeIntervalSince1970: TimeInterval(end))
            let day = Calendar.current.isDateInToday(s) ? "" : Sidebar.wake(start).components(separatedBy: " ").first.map { $0 + " " } ?? ""
            parts.append(day + "\(CalendarModel.clockTime(s))–\(CalendarModel.clockTime(e))")
        case .allDay:
            parts.append("All day")
        }
        if let provider = r.meetingProvider { parts.append(provider.name) } else if !r.location.isEmpty { parts.append(r.location) }
        return parts.joined(separator: " · ")
    }

    private nonisolated static func reminder(_ info: [AnyHashable: Any]) -> (String, Int64)? {
        guard let id = info["reminder"] as? String, let fireAt = (info["fireAt"] as? NSNumber)?.int64Value else { return nil }
        return (id, fireAt)
    }

    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse) async {
        let info = response.notification.request.content.userInfo
        if let thread = info["thread"] as? Int64 {
            await MainActor.run { onOpen?(thread) }
            return
        }
        guard let (id, fireAt) = Self.reminder(info) else { return }
        switch response.actionIdentifier {
        case Action.join:
            await MainActor.run { onJoin?(id, fireAt) }
        case Action.snooze:
            let content = response.notification.request.content
            let id = Self.snoozePrefix + response.notification.request.identifier
            let request = UNNotificationRequest(identifier: id, content: content, trigger: UNTimeIntervalNotificationTrigger(timeInterval: 300, repeats: false))
            try? await center.add(request)
        default:
            await MainActor.run { onOpenEvent?(id, fireAt) }
        }
    }

    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification) async -> UNNotificationPresentationOptions {
        [.banner, .sound]
    }
}
