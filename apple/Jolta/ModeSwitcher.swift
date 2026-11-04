@preconcurrency import JoltaFFI
import SwiftUI

/// Mail and Calendar, beside the window buttons in both modes, so the
/// calendar can be found without knowing ⌘2. In mail, a dot on Calendar
/// marks a meeting from 5 minutes before it starts until it ends; clicking
/// then opens that meeting.
struct ModeSwitcher: View {
    let model: AppModel

    /// From the window's left edge, so it doesn't move between the mail
    /// sidebar and the wider calendar one: right-aligned in the mail sidebar.
    static let leading = Theme.sidebarWidth - 10 - 54

    var body: some View {
        TimelineView(.periodic(from: .now, by: 30)) { context in
            HStack(spacing: 2) {
                button(.mail, icon: "envelope", title: "Mail", key: "⌘1", dot: false)
                button(.calendar, icon: "calendar", title: "Calendar", key: "⌘2", dot: meetingSoon(at: context.date) != nil)
            }
        }
    }

    private func meetingSoon(at now: Date) -> NextMeetingRecord? {
        guard model.mode == .mail, let next = model.calendar.nextMeeting else { return nil }
        let t = Int64(now.timeIntervalSince1970)
        return next.start - 5 * 60 <= t && t < next.end ? next : nil
    }

    private func button(_ mode: AppMode, icon: String, title: String, key: String, dot: Bool) -> some View {
        let on = model.mode == mode
        return Button {
            let meeting = mode == .calendar ? meetingSoon(at: .now) : nil
            model.mode = mode
            if let meeting {
                model.calendar.reveal(eventID: meeting.eventId, time: .timed(start: meeting.start, end: meeting.end))
            }
        } label: {
            Image(systemName: icon)
                .font(.system(size: 12.5, weight: .medium))
                .foregroundStyle(Color(on ? Theme.accent : Theme.text3))
                .frame(width: 26, height: 22)
                .background(RoundedRectangle(cornerRadius: 6).fill(Color(on ? Theme.fill : .clear)))
                .overlay(alignment: .topTrailing) {
                    if dot {
                        Circle().fill(Color(Theme.accent)).frame(width: 6, height: 6).offset(x: -3, y: 3)
                    }
                }
                .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .help(dot ? "\(title) \(key), a meeting is starting" : "\(title) \(key)")
        .accessibilityLabel(title)
        .accessibilityValue(on ? "selected" : (dot ? "meeting starting" : ""))
        .accessibilityIdentifier("mode-\(mode == .mail ? "mail" : "calendar")")
    }
}
