//! New-mail notifications for the app: the settings, the notices the core
//! sends as `CoreEvent::Notice`, and what a banner's buttons do. The app
//! only delivers; every decision is the core's (`jolta_core::notify`).

use jolta_core::actions::{Action, Context};
use jolta_core::notify;

use crate::{Core, Result};

/// Which new inbox mail gets a notification while Jolta runs.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotifyScope {
    Important,
    AllNewMail,
    Vips,
    Nothing,
}

/// What the Dock badge counts.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadgeScope {
    AllUnread,
    ImportantUnread,
    Nothing,
}

#[derive(uniffi::Record, Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotifySettings {
    pub scope: NotifyScope,
    pub sound: bool,
    pub badge: BadgeScope,
}

/// macOS passive, active and Time Sensitive.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeLevel {
    Quiet,
    Normal,
    Urgent,
}

#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeAction {
    Archive,
    MarkRead,
    Reply,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct NoticeContent {
    /// Showing an id again replaces the notification with that id.
    pub id: String,
    pub group: String,
    pub title: String,
    pub subtitle: String,
    pub body: String,
    pub level: NoticeLevel,
    pub sound: bool,
    /// The conversation a click opens; None opens the unified inbox.
    pub thread_id: Option<i64>,
    pub actions: Vec<NoticeAction>,
    pub account_id: Option<i64>,
    /// When the newest message behind it arrived, unix seconds.
    pub arrived: i64,
}

#[derive(uniffi::Enum, Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    Show { content: NoticeContent },
    Withdraw { ids: Vec<String> },
    Badge { count: u32 },
}

impl From<notify::Notice> for Notice {
    fn from(n: notify::Notice) -> Notice {
        match n {
            notify::Notice::Show(n) => Notice::Show {
                content: NoticeContent {
                    id: n.id,
                    group: n.group,
                    title: n.title,
                    subtitle: n.subtitle,
                    body: n.body,
                    level: match n.level {
                        notify::Level::Quiet => NoticeLevel::Quiet,
                        notify::Level::Normal => NoticeLevel::Normal,
                        notify::Level::Urgent => NoticeLevel::Urgent,
                    },
                    sound: n.sound,
                    thread_id: match n.open {
                        notify::Target::Thread(t) => Some(t.0),
                        notify::Target::UnifiedInbox => None,
                    },
                    actions: n.actions.into_iter().map(Into::into).collect(),
                    account_id: n.account.map(|a| a.0),
                    arrived: n.arrived,
                },
            },
            notify::Notice::Withdraw(ids) => Notice::Withdraw { ids },
            notify::Notice::Badge(count) => Notice::Badge { count },
        }
    }
}

impl From<notify::Action> for NoticeAction {
    fn from(a: notify::Action) -> NoticeAction {
        match a {
            notify::Action::Archive => NoticeAction::Archive,
            notify::Action::MarkRead => NoticeAction::MarkRead,
            notify::Action::Reply => NoticeAction::Reply,
        }
    }
}

impl From<notify::Settings> for NotifySettings {
    fn from(s: notify::Settings) -> NotifySettings {
        NotifySettings {
            scope: match s.scope {
                notify::Scope::Important => NotifyScope::Important,
                notify::Scope::All => NotifyScope::AllNewMail,
                notify::Scope::Vips => NotifyScope::Vips,
                notify::Scope::Nothing => NotifyScope::Nothing,
            },
            sound: s.sound,
            badge: match s.badge {
                notify::Badge::All => BadgeScope::AllUnread,
                notify::Badge::Important => BadgeScope::ImportantUnread,
                notify::Badge::Nothing => BadgeScope::Nothing,
            },
        }
    }
}

impl From<NotifySettings> for notify::Settings {
    fn from(s: NotifySettings) -> notify::Settings {
        notify::Settings {
            scope: match s.scope {
                NotifyScope::Important => notify::Scope::Important,
                NotifyScope::AllNewMail => notify::Scope::All,
                NotifyScope::Vips => notify::Scope::Vips,
                NotifyScope::Nothing => notify::Scope::Nothing,
            },
            sound: s.sound,
            badge: match s.badge {
                BadgeScope::AllUnread => notify::Badge::All,
                BadgeScope::ImportantUnread => notify::Badge::Important,
                BadgeScope::Nothing => notify::Badge::Nothing,
            },
        }
    }
}

#[uniffi::export]
impl Core {
    pub fn notify_settings(&self) -> Result<NotifySettings> {
        Ok(self.jolta.store.read(notify::Settings::read)?.into())
    }

    pub fn set_notify_settings(&self, settings: NotifySettings) -> Result<()> {
        let settings: notify::Settings = settings.into();
        self.jolta.store.write(|tx| settings.save(tx))?;
        self.tell_engine_settings_changed()
    }

    /// Moves the setting the app kept before ("all", "vips", "none") into
    /// the core, unless the core has one. Returns whether it did.
    pub fn migrate_new_mail_banners(&self, old: String) -> Result<bool> {
        let moved = self.jolta.store.write(|tx| notify::Settings::migrate(tx, &old))?;
        if moved {
            self.tell_engine_settings_changed()?;
        }
        Ok(moved)
    }

    /// The app became the active app, or stopped being it.
    pub fn set_foreground(&self, active: bool) {
        if let Some(engine) = self.engine.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            engine.notices().set_foreground(active);
        }
    }

    /// A banner's Archive or Mark as Read. Returns the tokens for `undo`;
    /// Reply and notifications that aren't one conversation's change nothing.
    pub fn notice_action(&self, id: String, action: NoticeAction) -> Result<Vec<i64>> {
        let Some(thread) = notify::thread_of(&id) else { return Ok(Vec::new()) };
        let action = match action {
            NoticeAction::Archive => Action::Archive,
            NoticeAction::MarkRead => Action::MarkRead,
            NoticeAction::Reply => return Ok(Vec::new()),
        };
        Ok(self.running_engine()?.perform(&[thread], action, Context::Inbox)?.into_iter().map(|op| op.0).collect())
    }

    /// The Dock badge's number now.
    pub fn badge_count(&self) -> Result<u32> {
        Ok(self.jolta.store.read(|c| notify::badge_count(c, &notify::Settings::read(c)?))?)
    }
}

impl Core {
    fn tell_engine_settings_changed(&self) -> Result<()> {
        if let Some(engine) = self.engine.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            engine.notices().settings_changed()?;
        }
        Ok(())
    }
}
