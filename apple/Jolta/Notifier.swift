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
    var onOpenEvent: ((Int64, OccurrenceTime?) -> Void)?
    var onJoin: ((Int64, OccurrenceTime?) -> Void)?

    /// Scheduled reminders' identifiers start with this; the record's id follows.
    static let reminderPrefix = "cal-"
    private nonisolated static let snoozePrefix = "calsnooze-"
    /// macOS keeps 64 pending notifications per app (measured on macOS 27,
    /// see docs/architecture.md); a few stay free for snoozes.
    static let reminderLimit = 56

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
    func schedule(reminders: [ReminderRecord]) async {
        let center = UNUserNotificationCenter.current()
        let now = Int64(Date.now.timeIntervalSince1970)
        let wanted = reminders.filter { $0.fireAt > now }.prefix(Self.reminderLimit)
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
            content.userInfo = ["event": r.eventId, "time": Self.encode(r.time)]
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

    private nonisolated static func encode(_ time: OccurrenceTime) -> [String: Any] {
        switch time {
        case .timed(let start, let end): ["start": start, "end": end]
        case .allDay(let first, let last): ["first": first, "last": last]
        }
    }

    private nonisolated static func decode(_ value: Any?) -> OccurrenceTime? {
        guard let d = value as? [String: Any] else { return nil }
        if let start = (d["start"] as? NSNumber)?.int64Value, let end = (d["end"] as? NSNumber)?.int64Value { return .timed(start: start, end: end) }
        if let first = d["first"] as? String, let last = d["last"] as? String { return .allDay(firstDay: first, lastDay: last) }
        return nil
    }

    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse) async {
        let info = response.notification.request.content.userInfo
        if let thread = info["thread"] as? Int64 {
            await MainActor.run { onOpen?(thread) }
            return
        }
        guard let event = (info["event"] as? NSNumber)?.int64Value else { return }
        let time = Self.decode(info["time"])
        switch response.actionIdentifier {
        case Action.join:
            await MainActor.run { onJoin?(event, time) }
        case Action.snooze:
            let content = response.notification.request.content
            let id = Self.snoozePrefix + response.notification.request.identifier
            let request = UNNotificationRequest(identifier: id, content: content, trigger: UNTimeIntervalNotificationTrigger(timeInterval: 300, repeats: false))
            try? await center.add(request)
        default:
            await MainActor.run { onOpenEvent?(event, time) }
        }
    }

    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification) async -> UNNotificationPresentationOptions {
        [.banner, .sound]
    }
}
