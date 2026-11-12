@preconcurrency import JoltaFFI
import SwiftUI

/// Today beside the mail: the meeting on now or next on top with its Join
/// button, then the day's hours around now. ⇧C shows and hides it.
struct DayPanel: View {
    let model: AppModel

    private static let date: DateFormatter = {
        let f = DateFormatter()
        f.setLocalizedDateFormatFromTemplate("EEE d MMM")
        return f
    }()

    var body: some View {
        let cal = model.calendar
        VStack(spacing: 0) {
            VStack(alignment: .leading, spacing: 10) {
                HStack(alignment: .firstTextBaseline) {
                    Text("Today").font(.system(size: 15, weight: .semibold))
                    Spacer()
                    Text(Self.date.string(from: cal.now)).font(.system(size: 12)).foregroundStyle(Color(Theme.text3))
                }
                if let card = UpNext(cal: cal) { card }
            }
            .padding(.horizontal, 16)
            .padding(.top, 16)
            .padding(.bottom, 12)
            .background(TitleBarArea())
            .overlay(alignment: .bottom) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
            let show = { (e: CalEvent) in
                model.mode = .calendar
                cal.reveal(eventID: e.record.eventId, time: e.record.time)
            }
            let hour = Double(Calendar.current.component(.hour, from: cal.now))
            WeekGrid(cal: cal, source: .today, gutter: 48, firstHour: min(max(hour - 1, 7.5), 14), surface: .side, select: show, open: show)
            HStack(spacing: 12) {
                HStack(spacing: 5) { KeyCap(key: "⇧C"); Text("panel") }
                HStack(spacing: 5) { KeyCap(key: "⌘2"); Text("calendar") }
                Spacer()
            }
            .font(.system(size: Theme.small))
            .foregroundStyle(Color(Theme.text3))
            .padding(.horizontal, 16)
            .frame(height: 34)
            .overlay(alignment: .top) { Rectangle().fill(Color(Theme.line2)).frame(height: 1) }
        }
        .background(Color(Theme.side))
        .overlay(alignment: .leading) { Rectangle().fill(Color(Theme.line2)).frame(width: 1) }
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Today")
    }
}

/// The card on top of the day panel: the online meeting to join when it is
/// today, else the event on now or next today.
struct UpNext: View {
    let cal: CalendarModel
    let label: String
    let title: String
    let detail: String
    let join: (() -> Void)?
    let live: Bool

    init?(cal: CalendarModel) {
        self.cal = cal
        let now = cal.now
        if let m = cal.nextMeeting, Calendar.current.isDate(Date(timeIntervalSince1970: TimeInterval(m.start)), inSameDayAs: now) {
            label = Self.label(start: m.start, end: m.end, now: now)
            title = m.title
            detail = m.meeting.provider.name
            join = { CalendarModel.join(m.meeting) }
            live = CalendarModel.isLive(start: m.start, end: m.end, now: now)
            return
        }
        guard let e = cal.todayEvents.first(where: { !$0.isLong && ($0.end ?? .distantPast) > now && !$0.isDeclined && !$0.isCancelled }),
              let start = e.start, let end = e.end else { return nil }
        label = Self.label(start: Int64(start.timeIntervalSince1970), end: Int64(end.timeIntervalSince1970), now: now)
        title = e.title
        detail = e.record.location
        join = e.record.joinUrl == nil ? nil : { cal.join(e) }
        live = CalendarModel.isLive(start: Int64(start.timeIntervalSince1970), end: Int64(end.timeIntervalSince1970), now: now)
    }

    /// "NOW, UNTIL 11:00", "IN 12 MIN", "NEXT, 14:00".
    static func label(start: Int64, end: Int64, now: Date) -> String {
        let t = Int64(now.timeIntervalSince1970)
        let clock = { (u: Int64) in CalendarModel.clockTime(Date(timeIntervalSince1970: TimeInterval(u))) }
        if t >= start && t < end { return "Now, until \(clock(end))" }
        if start - t < 3600 { return "In \(max(1, (start - t + 59) / 60)) min" }
        return "Next, \(clock(start))"
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(label.uppercased()).font(.system(size: 11, weight: .semibold)).tracking(0.2).foregroundStyle(Color(Theme.text3))
            Text(title).fontWeight(.semibold).lineLimit(2)
            HStack {
                Text(detail).font(.system(size: 12)).foregroundStyle(Color(Theme.text2)).lineLimit(1)
                Spacer()
                if let join {
                    KitButton(title: "Join", kind: live ? .primary : .secondary, size: .small, action: join)
                        .help("Join the online meeting")
                }
            }
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .card()
        .accessibilityElement(children: .combine)
    }
}
