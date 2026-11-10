//! Decides which new mail gets a notification, when, and with what words.
//! Plain state and a clock passed in, no I/O, so every rule is a unit test.
//! The engine feeds it mail as sync rounds find it, asks when to come back
//! (`due`), and hands the shell what it returns.

use std::collections::{BTreeMap, HashSet, VecDeque};

use super::{Action, Level, Notice, Notification, Scope, Settings, Target};
use crate::model::*;

/// A gather closes this long after the last new message...
pub const QUIET: i64 = 5;
/// ...or this long after the first, whichever comes first. Mail waiting
/// for AI sorting waits this long.
pub const GATHER: i64 = 20;
/// Mail that arrived longer ago than this when it is decided on (the Mac
/// slept, Jolta was quit) only reaches the summary.
pub const LATE: i64 = 10 * 60;
/// Conversations that get their own banner in one flush.
pub const BANNERS_AT_ONCE: usize = 3;
/// Own banners within `BANNER_WINDOW`.
pub const BANNERS_PER_WINDOW: usize = 6;
pub const BANNER_WINDOW: i64 = 10 * 60;
/// At most one sound this often.
pub const SOUND_GAP: i64 = 60;
/// One urgent notification per sender this often, and this many an hour.
pub const URGENT_PER_SENDER: i64 = 10 * 60;
pub const URGENT_PER_HOUR: usize = 4;
/// More reminders than this in one round become one notification.
pub const REMINDERS_AT_ONCE: usize = 3;
/// A banner's body is at most this many characters.
pub const BODY: usize = 140;

const SUMMARY: &str = "mail-summary";

/// A new inbox message as the store sees it now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub message: MessageId,
    pub thread: ThreadId,
    pub account: AccountId,
    /// The sender's name, or the address when there is none.
    pub sender: String,
    pub sender_address: String,
    pub subject: String,
    pub first_line: String,
    /// When the server received it.
    pub arrived: i64,
    pub important: bool,
    pub vip: bool,
    /// Urgent by a rule the AI plays no part in: a VIP writing to the user,
    /// a reply to the user in a conversation they started, or a meeting
    /// starting within 4 hours.
    pub urgent: bool,
    /// AI sorting hasn't answered for it yet and is running.
    pub sorting: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reminder {
    /// Back from snooze.
    Snooze,
    /// Nobody replied in time.
    NoReply,
}

pub struct Planner {
    settings: Settings,
    foreground: bool,
    /// Mail waiting for the gather to close, and when it began and last grew.
    gather: Vec<Candidate>,
    first: i64,
    last: i64,
    /// Conversations with a banner of their own, and the mail behind each.
    banners: BTreeMap<ThreadId, Vec<Candidate>>,
    /// The mail behind the summary, each with whether it was late.
    summary: Vec<(Candidate, bool)>,
    /// When own banners were shown, within `BANNER_WINDOW`.
    banner_times: VecDeque<i64>,
    /// Urgent notifications in the last hour, by sender address.
    urgent_times: VecDeque<(String, i64)>,
    last_sound: Option<i64>,
    /// Every message ever taken in, so mail reported again isn't announced twice.
    seen: HashSet<MessageId>,
}

/// Where a message can go under the user's choice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reach {
    Banner,
    Summary,
}

impl Planner {
    pub fn new(settings: Settings) -> Planner {
        Planner {
            settings,
            foreground: false,
            gather: Vec::new(),
            first: 0,
            last: 0,
            banners: BTreeMap::new(),
            summary: Vec::new(),
            banner_times: VecDeque::new(),
            urgent_times: VecDeque::new(),
            last_sound: None,
            seen: HashSet::new(),
        }
    }

    pub fn set_settings(&mut self, settings: Settings) {
        self.settings = settings;
    }

    /// The app became the active app, or stopped being it.
    pub fn set_foreground(&mut self, active: bool) {
        self.foreground = active;
    }

    /// New mail from a sync round. Urgent mail is shown at once; the rest
    /// waits for the gather to close.
    pub fn arrive(&mut self, mail: Vec<Candidate>, now: i64) -> Vec<Notice> {
        let mut out = Vec::new();
        if self.settings.scope == Scope::Nothing {
            return out;
        }
        for c in mail {
            if !self.seen.insert(c.message) || (self.reach(&c).is_none() && !c.sorting) {
                continue;
            }
            if self.urgent_now(&c, now) {
                self.urgent_times.push_back((c.sender_address.to_lowercase(), now));
                let sound = self.sound(now);
                let thread = self.banners.entry(c.thread).or_default();
                thread.push(c);
                out.push(Notice::Show(banner(thread, Level::Urgent, sound)));
                continue;
            }
            if self.gather.is_empty() {
                self.first = now;
            }
            self.last = now;
            self.gather.push(c);
        }
        out
    }

    /// The mail waiting in the gather, for the store to look at again.
    pub fn pending(&self) -> Vec<MessageId> {
        self.gather.iter().map(|c| c.message).collect()
    }

    /// The waiting mail as the store sees it now. Mail missing from `fresh`
    /// was read or left the inbox and is dropped.
    pub fn refresh(&mut self, fresh: Vec<Candidate>) {
        let mut fresh: std::collections::HashMap<MessageId, Candidate> = fresh.into_iter().map(|c| (c.message, c)).collect();
        self.gather = self.gather.iter().filter_map(|c| fresh.remove(&c.message)).collect();
    }

    /// When the gather closes, if anything waits.
    pub fn due(&self) -> Option<i64> {
        if self.gather.is_empty() {
            return None;
        }
        let latest = self.first + GATHER;
        Some(if self.gather.iter().any(|c| c.sorting) { latest } else { (self.last + QUIET).min(latest) })
    }

    /// Closes the gather when it is due: up to `BANNERS_AT_ONCE` new
    /// conversations get a banner, newest first, within the rolling limit;
    /// a conversation that has one gets it replaced; the rest, and mail
    /// that arrived while away, goes into the summary.
    pub fn tick(&mut self, now: i64) -> Vec<Notice> {
        if self.due().is_none_or(|due| due > now) {
            return Vec::new();
        }
        let gather = std::mem::take(&mut self.gather);
        if self.foreground {
            return Vec::new();
        }
        let mut replaced: Vec<ThreadId> = Vec::new();
        let mut fresh: BTreeMap<ThreadId, Vec<Candidate>> = BTreeMap::new();
        let mut to_summary: Vec<(Candidate, bool)> = Vec::new();
        for c in gather {
            let Some(reach) = self.reach(&c) else { continue };
            let late = now - c.arrived > LATE;
            if late || (reach == Reach::Summary && !self.banners.contains_key(&c.thread)) {
                to_summary.push((c, late));
            } else if let Some(shown) = self.banners.get_mut(&c.thread) {
                if !replaced.contains(&c.thread) {
                    replaced.push(c.thread);
                }
                shown.push(c);
            } else {
                fresh.entry(c.thread).or_default().push(c);
            }
        }
        let mut fresh: Vec<Vec<Candidate>> = fresh.into_values().collect();
        fresh.sort_by_key(|mail| std::cmp::Reverse(newest(mail).map(|c| (c.arrived, c.message))));

        let mut out = Vec::new();
        for thread in replaced {
            let sound = self.sound(now);
            out.push(Notice::Show(banner(&self.banners[&thread], Level::Normal, sound)));
        }
        self.banner_times.retain(|t| now - t < BANNER_WINDOW);
        let room = BANNERS_AT_ONCE.min(BANNERS_PER_WINDOW.saturating_sub(self.banner_times.len()));
        for (i, mail) in fresh.into_iter().enumerate() {
            if i >= room {
                to_summary.extend(mail.into_iter().map(|c| (c, false)));
                continue;
            }
            self.banner_times.push_back(now);
            let sound = self.sound(now);
            let thread = mail[0].thread;
            out.push(Notice::Show(banner(&mail, Level::Normal, sound)));
            self.banners.insert(thread, mail);
        }
        if !to_summary.is_empty() {
            self.summary.extend(to_summary);
            out.push(Notice::Show(self.summary_notification(false)));
        }
        out
    }

    /// The mail behind the notifications shown, for the store to check.
    pub fn shown(&self) -> Vec<MessageId> {
        self.banners.values().flatten().chain(self.summary.iter().map(|(c, _)| c)).map(|c| c.message).collect()
    }

    /// Takes away what no longer stands: a banner once every message behind
    /// it is read or gone from the inbox, and those messages from the summary.
    /// `still` is the shown mail that is still unread in the inbox.
    pub fn withdraw(&mut self, still: &HashSet<MessageId>) -> Vec<Notice> {
        let mut gone = Vec::new();
        self.banners.retain(|thread, mail| {
            let keep = mail.iter().any(|c| still.contains(&c.message));
            if !keep {
                gone.push(format!("mail-{thread}"));
            }
            keep
        });
        let mut out = Vec::new();
        let before = self.summary.len();
        self.summary.retain(|(c, _)| still.contains(&c.message));
        if self.summary.len() != before {
            if self.summary.is_empty() {
                gone.push(SUMMARY.into());
            } else {
                out.push(Notice::Show(self.summary_notification(true)));
            }
        }
        if !gone.is_empty() {
            out.insert(0, Notice::Withdraw(gone));
        }
        out
    }

    /// Reminders the user asked for, which always get a banner.
    pub fn remind(&mut self, kind: Reminder, threads: Vec<(AccountId, ThreadId, String)>, now: i64) -> Vec<Notice> {
        if self.settings.scope == Scope::Nothing || threads.is_empty() {
            return Vec::new();
        }
        if threads.len() > REMINDERS_AT_ONCE {
            let subjects: Vec<&str> = threads.iter().map(|(_, _, s)| shown_subject(s)).collect();
            return vec![Notice::Show(Notification {
                id: "reminders".into(),
                group: "reminders".into(),
                title: format!("{} conversations are back", threads.len()),
                subtitle: String::new(),
                body: clip(&subjects.join(", ")),
                level: Level::Normal,
                sound: self.sound(now),
                open: Target::UnifiedInbox,
                actions: Vec::new(),
                account: None,
                arrived: now,
            })];
        }
        let (prefix, title) = match kind {
            Reminder::Snooze => ("snooze", "Back from snooze"),
            Reminder::NoReply => ("noreply", "No reply yet"),
        };
        threads
            .into_iter()
            .map(|(account, thread, subject)| {
                Notice::Show(Notification {
                    id: format!("{prefix}-{thread}"),
                    group: "reminders".into(),
                    title: title.into(),
                    subtitle: shown_subject(&subject).into(),
                    body: String::new(),
                    level: Level::Normal,
                    sound: self.sound(now),
                    open: Target::Thread(thread),
                    actions: vec![Action::Archive, Action::MarkRead, Action::Reply],
                    account: Some(account),
                    arrived: now,
                })
            })
            .collect()
    }

    fn reach(&self, c: &Candidate) -> Option<Reach> {
        match self.settings.scope {
            Scope::Important => c.important.then_some(Reach::Banner),
            Scope::All => Some(if c.important { Reach::Banner } else { Reach::Summary }),
            Scope::Vips => c.vip.then_some(Reach::Banner),
            Scope::Nothing => None,
        }
    }

    /// Urgent mail that arrived just now, within both caps.
    fn urgent_now(&mut self, c: &Candidate, now: i64) -> bool {
        if !(c.urgent && c.important && self.reach(c) == Some(Reach::Banner) && now - c.arrived <= LATE) {
            return false;
        }
        while self.urgent_times.front().is_some_and(|(_, t)| now - t >= 3600) {
            self.urgent_times.pop_front();
        }
        let sender = c.sender_address.to_lowercase();
        self.urgent_times.len() < URGENT_PER_HOUR
            && !self.urgent_times.iter().any(|(s, t)| *s == sender && now - t < URGENT_PER_SENDER)
    }

    /// Whether this notification plays a sound, and if so, notes it.
    fn sound(&mut self, now: i64) -> bool {
        if !self.settings.sound || self.foreground || self.last_sound.is_some_and(|t| now - t < SOUND_GAP) {
            return false;
        }
        self.last_sound = Some(now);
        true
    }

    /// "5 new messages", from the important senders first. Quiet when it
    /// holds no important mail, or when it only lost messages.
    fn summary_notification(&self, shrunk: bool) -> Notification {
        let n = self.summary.len();
        let away = if self.summary.iter().all(|(_, late)| *late) { " while you were away" } else { "" };
        let mut mail: Vec<&Candidate> = self.summary.iter().map(|(c, _)| c).collect();
        mail.sort_by_key(|c| std::cmp::Reverse((c.important, c.arrived, c.message)));
        let mut names: Vec<&str> = Vec::new();
        for c in &mail {
            if !names.contains(&c.sender.as_str()) {
                names.push(&c.sender);
            }
        }
        let important = self.summary.iter().any(|(c, _)| c.important);
        Notification {
            id: SUMMARY.into(),
            group: "mail".into(),
            title: format!("{n} new message{}{away}", if n == 1 { "" } else { "s" }),
            subtitle: String::new(),
            body: from(&names),
            level: if important && !shrunk { Level::Normal } else { Level::Quiet },
            sound: false,
            open: Target::UnifiedInbox,
            actions: Vec::new(),
            account: None,
            arrived: mail.iter().map(|c| c.arrived).max().unwrap_or_default(),
        }
    }
}

fn newest(mail: &[Candidate]) -> Option<&Candidate> {
    mail.iter().max_by_key(|c| (c.arrived, c.message))
}

/// A conversation's banner: the newest sender, how many messages, the
/// latest subject and its first line.
fn banner(mail: &[Candidate], level: Level, sound: bool) -> Notification {
    let c = newest(mail).expect("a banner has mail behind it");
    let title = if mail.len() == 1 { c.sender.clone() } else { format!("{}, {} new messages", c.sender, mail.len()) };
    Notification {
        id: format!("mail-{}", c.thread),
        group: format!("mail-{}", c.thread),
        title,
        subtitle: shown_subject(&c.subject).into(),
        body: clip(&c.first_line),
        level,
        sound,
        open: Target::Thread(c.thread),
        actions: vec![Action::Archive, Action::MarkRead, Action::Reply],
        account: Some(c.account),
        arrived: c.arrived,
    }
}

fn shown_subject(subject: &str) -> &str {
    if subject.trim().is_empty() { "(no subject)" } else { subject }
}

fn clip(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= BODY {
        return text.to_string();
    }
    let cut: String = text.chars().take(BODY - 1).collect();
    format!("{}…", cut.trim_end())
}

/// "From Ann", "From Ann and Bob", "From Ann, Bob and Cas", "From Ann, Bob and 3 others".
fn from(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => format!("From {one}"),
        [a, b] => format!("From {a} and {b}"),
        [a, b, c] => format!("From {a}, {b} and {c}"),
        [a, b, rest @ ..] => format!("From {a}, {b} and {} others", rest.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::Badge;

    const T0: i64 = 1_800_000_000;

    fn mail(id: i64, thread: i64, sender: &str, arrived: i64) -> Candidate {
        Candidate {
            message: MessageId(id),
            thread: ThreadId(thread),
            account: AccountId(1),
            sender: sender.into(),
            sender_address: format!("{}@test.invalid", sender.to_lowercase().replace(' ', ".")),
            subject: format!("Subject {id}"),
            first_line: format!("First line of {id}"),
            arrived,
            important: true,
            vip: false,
            urgent: false,
            sorting: false,
        }
    }

    impl Candidate {
        fn unimportant(mut self) -> Candidate {
            self.important = false;
            self
        }
        fn vip(mut self) -> Candidate {
            self.vip = true;
            self
        }
        fn urgent(mut self) -> Candidate {
            self.urgent = true;
            self
        }
        fn sorting(mut self) -> Candidate {
            self.sorting = true;
            self
        }
    }

    fn planner(scope: Scope) -> Planner {
        Planner::new(Settings { scope, sound: true, badge: Badge::All })
    }

    fn shows(notices: &[Notice]) -> Vec<&Notification> {
        notices
            .iter()
            .filter_map(|n| match n {
                Notice::Show(n) => Some(n),
                _ => None,
            })
            .collect()
    }

    fn ids(notices: &[Notice]) -> Vec<String> {
        shows(notices).iter().map(|n| n.id.clone()).collect()
    }

    /// Arrives each message alone at its own arrival time, then lets the gather close.
    fn run(p: &mut Planner, mail: Vec<Candidate>) -> Vec<Notice> {
        let mut out = Vec::new();
        for m in mail {
            let at = m.arrived.max(T0);
            out.extend(p.arrive(vec![m], at));
        }
        let due = p.due().expect("something waits");
        out.extend(p.tick(due));
        out
    }

    #[test]
    fn one_message_is_one_banner_with_sound_after_a_quiet_spell() {
        let mut p = planner(Scope::Important);
        assert!(p.arrive(vec![mail(1, 10, "Ann Smith", T0)], T0).is_empty(), "nothing before the gather closes");
        assert_eq!(p.due(), Some(T0 + QUIET));
        assert!(p.tick(T0 + QUIET - 1).is_empty());
        let out = p.tick(T0 + QUIET);
        let shown = shows(&out);
        assert_eq!(shown.len(), 1);
        let n = shown[0];
        assert_eq!((n.id.as_str(), n.title.as_str(), n.subtitle.as_str(), n.body.as_str()), ("mail-10", "Ann Smith", "Subject 1", "First line of 1"));
        assert_eq!((n.level, n.sound, n.open), (Level::Normal, true, Target::Thread(ThreadId(10))));
        assert_eq!(n.actions, [Action::Archive, Action::MarkRead, Action::Reply]);
        assert_eq!(p.due(), None);
    }

    #[test]
    fn the_gather_waits_for_five_quiet_seconds_but_no_more_than_twenty() {
        let mut p = planner(Scope::Important);
        for (i, at) in [0, 4, 8, 12, 16].into_iter().enumerate() {
            p.arrive(vec![mail(i as i64, i as i64, "Ann", T0 + at)], T0 + at);
        }
        assert_eq!(p.due(), Some(T0 + GATHER), "mail every 4 seconds closes at 20 seconds");
        assert!(p.tick(T0 + 19).is_empty());
        assert!(!p.tick(T0 + GATHER).is_empty());
    }

    #[test]
    fn thirty_list_messages_under_all_new_mail_are_one_silent_summary() {
        let all: Vec<Candidate> = (0..30).map(|i| mail(i, 100 + i, "Shop", T0 + i / 3).unimportant()).collect();
        let mut p = planner(Scope::All);
        let out = run(&mut p, all.clone());
        assert_eq!(ids(&out), ["mail-summary"]);
        let summary = shows(&out)[0];
        assert_eq!((summary.title.as_str(), summary.level, summary.sound), ("30 new messages", Level::Quiet, false));
        assert_eq!(summary.open, Target::UnifiedInbox);

        let mut p = planner(Scope::Important);
        for m in all {
            assert!(p.arrive(vec![m], T0).is_empty());
        }
        assert!(p.tick(T0 + GATHER).is_empty(), "under Important mail they get nothing");
    }

    #[test]
    fn mail_in_one_conversation_replaces_its_banner() {
        let mut p = planner(Scope::Important);
        let out = run(&mut p, vec![mail(1, 10, "Ann Smith", T0), mail(2, 10, "Ann Smith", T0 + 1), mail(3, 10, "Ann Smith", T0 + 2)]);
        assert_eq!(ids(&out), ["mail-10"]);
        assert_eq!(shows(&out)[0].title, "Ann Smith, 3 new messages");
        assert_eq!(shows(&out)[0].subtitle, "Subject 3", "the latest subject");

        // Fill the window of own banners; the conversation's next message still replaces its banner.
        for round in 0..2 {
            let base = T0 + 100 + round * 60;
            run(&mut p, (0..3).map(|i| mail(20 + round * 3 + i, 20 + round * 3 + i, "Bob", base)).collect());
        }
        let out = run(&mut p, vec![mail(4, 10, "Ann Smith", T0 + 300)]);
        assert_eq!(ids(&out), ["mail-10"]);
        assert_eq!(shows(&out)[0].title, "Ann Smith, 4 new messages");
    }

    #[test]
    fn five_conversations_at_once_are_three_banners_and_a_summary_of_two() {
        let mut p = planner(Scope::Important);
        let names = ["Ann", "Bob", "Cas", "Dee", "Eli"];
        let mail: Vec<Candidate> = names.iter().enumerate().map(|(i, n)| mail(i as i64, i as i64, n, T0 + i as i64)).collect();
        let mut out = p.arrive(mail, T0 + 4);
        out.extend(p.tick(T0 + 4 + QUIET));
        assert_eq!(ids(&out), ["mail-4", "mail-3", "mail-2", "mail-summary"], "the newest three get banners");
        let summary = shows(&out)[3];
        assert_eq!((summary.title.as_str(), summary.body.as_str()), ("2 new messages", "From Bob and Ann"));
        assert_eq!((summary.level, summary.sound), (Level::Normal, false), "important mail inside, but silent");
        let sounds: Vec<bool> = shows(&out).iter().map(|n| n.sound).collect();
        assert_eq!(sounds, [true, false, false, false], "one sound for the flush");
    }

    #[test]
    fn the_summary_names_important_senders_first() {
        let mut p = planner(Scope::All);
        let mut all = vec![mail(1, 1, "Ann Smith", T0)];
        all.extend((2..6).map(|i| mail(i, i, &format!("List {i}"), T0).unimportant()));
        all.extend((6..9).map(|i| mail(i, i, &format!("Person {i}"), T0 - 2 * LATE + i)));
        let mut out = p.arrive(all, T0);
        out.extend(p.tick(T0 + QUIET));
        assert_eq!(ids(&out), ["mail-1", "mail-summary"]);
        let summary = shows(&out)[1];
        assert_eq!(summary.title, "7 new messages");
        assert_eq!(summary.body, "From Person 8, Person 7 and 5 others");
    }

    #[test]
    fn mail_that_arrived_while_away_only_reaches_the_summary() {
        let mut p = planner(Scope::Important);
        let away = vec![mail(1, 1, "Ann", T0 - 3600), mail(2, 2, "Vera", T0 - 3600).vip().urgent()];
        let out = p.arrive(away, T0);
        assert!(out.is_empty(), "late urgent mail is not urgent");
        let out = p.tick(T0 + QUIET);
        assert_eq!(ids(&out), ["mail-summary"]);
        let n = shows(&out)[0];
        assert_eq!((n.title.as_str(), n.sound), ("2 new messages while you were away", false));

        let out = run(&mut p, vec![mail(3, 3, "Bob", T0 - LATE - 100)]);
        assert_eq!(shows(&out)[0].title, "3 new messages while you were away");
        let out = run(&mut p, vec![mail(4, 4, "Cas", T0 - 3600)]);
        assert_eq!(ids(&out), ["mail-summary"]);
    }

    #[test]
    fn at_most_one_sound_a_minute_and_none_when_switched_off() {
        let mut p = planner(Scope::Important);
        let sound = |out: &[Notice]| shows(out).iter().any(|n| n.sound);
        assert!(sound(&run(&mut p, vec![mail(1, 1, "Ann", T0)])));
        assert!(!sound(&run(&mut p, vec![mail(2, 2, "Bob", T0 + 30)])));
        assert!(sound(&run(&mut p, vec![mail(3, 3, "Cas", T0 + 70)])));
        p.set_settings(Settings { sound: false, ..p.settings });
        assert!(!sound(&run(&mut p, vec![mail(4, 4, "Dee", T0 + 500)])));
        assert!(!sound(&p.arrive(vec![mail(5, 5, "Vera", T0 + 600).vip().urgent()], T0 + 600)));
    }

    #[test]
    fn six_banners_in_ten_minutes_then_the_summary() {
        let mut p = planner(Scope::Important);
        let first = run(&mut p, (1..4).map(|i| mail(i, i, "Ann", T0)).collect());
        let second = run(&mut p, (4..7).map(|i| mail(i, i, "Bob", T0 + 60)).collect());
        assert_eq!(ids(&first).len() + ids(&second).len(), 6);
        assert_eq!(ids(&run(&mut p, vec![mail(7, 7, "Cas", T0 + 120)])), ["mail-summary"], "the seventh waits in the summary");
        assert_eq!(ids(&run(&mut p, vec![mail(8, 8, "Dee", T0 + QUIET + BANNER_WINDOW)])), ["mail-8"], "the window frees up");
    }

    #[test]
    fn urgent_mail_skips_the_wait_within_its_caps() {
        let mut p = planner(Scope::Important);
        let out = p.arrive(vec![mail(1, 1, "Vera", T0).vip().urgent()], T0);
        assert_eq!(ids(&out), ["mail-1"], "no wait");
        assert_eq!((shows(&out)[0].level, shows(&out)[0].sound), (Level::Urgent, true));

        assert!(p.arrive(vec![mail(2, 2, "Vera", T0 + 60).vip().urgent()], T0 + 60).is_empty(), "the same sender within 10 minutes waits");
        let out = p.tick(T0 + 60 + QUIET);
        assert_eq!(shows(&out)[0].level, Level::Normal, "and is only important");

        for (i, who) in ["Wim", "Xia", "Yan"].into_iter().enumerate() {
            let at = T0 + 600 + i as i64;
            assert_eq!(ids(&p.arrive(vec![mail(10 + i as i64, 10 + i as i64, who, at).urgent()], at)).len(), 1);
        }
        assert!(p.arrive(vec![mail(20, 20, "Zoe", T0 + 900).urgent()], T0 + 900).is_empty(), "the fifth urgent in an hour waits");
        assert_eq!(ids(&p.arrive(vec![mail(21, 21, "Zoe", T0 + 3601).urgent()], T0 + 3601)), ["mail-21"]);
    }

    #[test]
    fn urgent_needs_important_mail() {
        let mut p = planner(Scope::Important);
        let asks = mail(1, 1, "Ann", T0).urgent().unimportant();
        assert!(p.arrive(vec![asks], T0).is_empty());
        assert!(p.tick(T0 + QUIET).is_empty());
    }

    #[test]
    fn while_jolta_is_in_front_only_urgent_mail_shows_silently() {
        let mut p = planner(Scope::All);
        p.set_foreground(true);
        assert!(run(&mut p, vec![mail(1, 1, "Ann", T0), mail(2, 2, "Shop", T0).unimportant()]).is_empty());
        let out = p.arrive(vec![mail(3, 3, "Vera", T0 + 10).vip().urgent()], T0 + 10);
        assert_eq!(ids(&out), ["mail-3"]);
        assert!(!shows(&out)[0].sound);
        p.set_foreground(false);
        assert_eq!(ids(&run(&mut p, vec![mail(4, 4, "Bob", T0 + 20)])), ["mail-4"]);
    }

    #[test]
    fn read_mail_takes_its_notification_away() {
        let mut p = planner(Scope::Important);
        let mut out = p.arrive((1..6).map(|i| mail(i, i, &format!("P{i}"), T0 + i)).collect(), T0 + 5);
        out.extend(p.tick(T0 + 5 + QUIET));
        assert_eq!(ids(&out), ["mail-5", "mail-4", "mail-3", "mail-summary"]);

        let still: HashSet<MessageId> = [3, 4, 2].map(MessageId).into();
        let out = p.withdraw(&still);
        assert!(out.contains(&Notice::Withdraw(vec!["mail-5".into()])));
        let summary = shows(&out)[0];
        assert_eq!((summary.title.as_str(), summary.level, summary.sound), ("1 new message", Level::Quiet, false), "the summary shrinks quietly");

        let out = p.withdraw(&HashSet::new());
        let mut gone: Vec<String> = out
            .iter()
            .flat_map(|n| match n {
                Notice::Withdraw(ids) => ids.clone(),
                _ => Vec::new(),
            })
            .collect();
        gone.sort();
        assert_eq!(gone, ["mail-3", "mail-4", "mail-summary"]);
        assert!(p.withdraw(&HashSet::new()).is_empty());
    }

    #[test]
    fn mail_read_before_the_gather_closes_gets_nothing() {
        let mut p = planner(Scope::Important);
        p.arrive(vec![mail(1, 1, "Ann", T0)], T0);
        p.refresh(Vec::new());
        assert!(p.tick(T0 + QUIET).is_empty());
        assert_eq!(p.due(), None);
    }

    #[test]
    fn mail_reported_twice_is_announced_once() {
        let mut p = planner(Scope::Important);
        assert_eq!(ids(&run(&mut p, vec![mail(1, 1, "Ann", T0)])).len(), 1);
        assert!(p.arrive(vec![mail(1, 1, "Ann", T0)], T0 + 30).is_empty());
        assert_eq!(p.due(), None);
    }

    #[test]
    fn only_vips_and_none() {
        let mut p = planner(Scope::Vips);
        let out = run(&mut p, vec![mail(1, 1, "Ann", T0), mail(2, 2, "Vera", T0).vip()]);
        assert_eq!(ids(&out), ["mail-2"]);

        let mut p = planner(Scope::Nothing);
        assert!(p.arrive(vec![mail(1, 1, "Vera", T0).vip().urgent()], T0).is_empty());
        assert_eq!(p.due(), None);
        assert!(p.remind(Reminder::Snooze, vec![(AccountId(1), ThreadId(1), "Taxes".into())], T0).is_empty());
    }

    #[test]
    fn mail_waits_for_ai_sorting_within_the_gather() {
        let mut p = planner(Scope::Important);
        p.arrive(vec![mail(1, 1, "Ann", T0).unimportant().sorting()], T0);
        assert_eq!(p.due(), Some(T0 + GATHER), "unsorted mail waits for its answer");
        p.refresh(vec![mail(1, 1, "Ann", T0)]);
        assert_eq!(p.due(), Some(T0 + QUIET), "sorted important: the usual wait");
        assert_eq!(ids(&p.tick(T0 + QUIET)), ["mail-1"]);

        p.arrive(vec![mail(2, 2, "Bob", T0 + 100).unimportant().sorting()], T0 + 100);
        assert!(p.tick(T0 + 100 + GATHER).is_empty(), "no answer in time: decided without it");
    }

    #[test]
    fn reminders_get_banners_and_many_become_one() {
        let mut p = planner(Scope::Important);
        let out = p.remind(Reminder::Snooze, vec![(AccountId(1), ThreadId(7), "Taxes".into())], T0);
        assert_eq!(ids(&out), ["snooze-7"]);
        let n = shows(&out)[0];
        assert_eq!((n.title.as_str(), n.subtitle.as_str(), n.sound), ("Back from snooze", "Taxes", true));
        let out = p.remind(Reminder::NoReply, vec![(AccountId(1), ThreadId(8), "Quote".into())], T0 + 5);
        assert_eq!(ids(&out), ["noreply-8"]);
        assert_eq!((shows(&out)[0].title.as_str(), shows(&out)[0].sound), ("No reply yet", false), "within a minute of the last sound");
        let many: Vec<_> = (0..6).map(|i| (AccountId(1), ThreadId(20 + i), format!("S{i}"))).collect();
        let out = p.remind(Reminder::Snooze, many, T0 + 3600);
        assert_eq!(ids(&out), ["reminders"]);
        assert_eq!(shows(&out)[0].title, "6 conversations are back");
    }
}
