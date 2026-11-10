//! New-mail notifications for the app: the settings, the notices the core
//! sends as `CoreEvent::Notice`, and what a banner's buttons do. The app
//! only delivers; every decision is the core's (`jolta_core::notify`).

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
        let _ = n;
        todo!()
    }
}

#[uniffi::export]
impl Core {
    pub fn notify_settings(&self) -> Result<NotifySettings> {
        todo!()
    }

    pub fn set_notify_settings(&self, settings: NotifySettings) -> Result<()> {
        let _ = settings;
        todo!()
    }

    /// Moves the setting the app kept before ("all", "vips", "none") into
    /// the core, unless the core has one. Returns whether it did.
    pub fn migrate_new_mail_banners(&self, old: String) -> Result<bool> {
        let _ = old;
        todo!()
    }

    /// The app became the active app, or stopped being it.
    pub fn set_foreground(&self, active: bool) {
        let _ = active;
        todo!()
    }

    /// A banner's Archive or Mark as Read. Returns the tokens for `undo`;
    /// Reply and notifications that aren't one conversation's change nothing.
    pub fn notice_action(&self, id: String, action: NoticeAction) -> Result<Vec<i64>> {
        let _ = (id, action);
        todo!()
    }

    /// The Dock badge's number now.
    pub fn badge_count(&self) -> Result<u32> {
        todo!()
    }
}
