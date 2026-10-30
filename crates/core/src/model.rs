use std::fmt;
use std::str::FromStr;

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub i64);

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                self.0.to_sql()
            }
        }

        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                i64::column_result(value).map($name)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(AccountId);
id_type!(MailboxId);
id_type!(MessageId);
id_type!(ThreadId);

macro_rules! text_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name { $($variant),+ }

        impl $name {
            pub const ALL: &[$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self { $($name::$variant => $text),+ }
            }
        }

        impl FromStr for $name {
            type Err = String;
            fn from_str(s: &str) -> Result<Self, String> {
                match s { $($text => Ok($name::$variant),)+ _ => Err(format!("unknown {}: {s}", stringify!($name))) }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(ToSqlOutput::from(self.as_str()))
            }
        }

        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                value.as_str()?.parse().map_err(|e: String| FromSqlError::Other(e.into()))
            }
        }
    };
}

text_enum!(AccountKind {
    Imap => "imap",
    Gmail => "gmail",
    Graph => "graph",
});

text_enum!(MailboxRole {
    Inbox => "inbox",
    Sent => "sent",
    Drafts => "drafts",
    Archive => "archive",
    Trash => "trash",
    Spam => "spam",
    Snoozed => "snoozed",
    All => "all",
});

/// IMAP-style message flags, stored as a bit set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags(pub u32);

impl Flags {
    pub const SEEN: u32 = 1;
    pub const FLAGGED: u32 = 2;
    pub const ANSWERED: u32 = 4;
    pub const DRAFT: u32 = 8;
    pub const DELETED: u32 = 16;

    pub fn has(self, bit: u32) -> bool {
        self.0 & bit != 0
    }

    pub fn with(self, bit: u32, on: bool) -> Flags {
        if on { Flags(self.0 | bit) } else { Flags(self.0 & !bit) }
    }
}

#[derive(Clone, Debug)]
pub struct Account {
    pub id: AccountId,
    pub kind: AccountKind,
    pub address: String,
    pub display_name: String,
    pub username: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub secret_ref: String,
}

#[derive(Clone, Debug)]
pub struct Mailbox {
    pub id: MailboxId,
    pub account_id: AccountId,
    pub remote_id: String,
    pub name: String,
    pub role: Option<MailboxRole>,
    pub delimiter: Option<String>,
    pub selectable: bool,
    pub uidvalidity: Option<u32>,
    pub highest_modseq: Option<u64>,
}

/// Header fields the store keeps for every message.
#[derive(Clone, Debug, Default)]
pub struct Envelope {
    pub message_id: Option<String>,
    pub in_reply_to: Vec<String>,
    pub references: Vec<String>,
    pub from_name: String,
    pub from_addr: String,
    pub to: String,
    pub cc: String,
    pub subject: String,
    pub list_id: Option<String>,
    pub has_attachments: bool,
}

/// What a downloaded body adds to a message.
#[derive(Clone, Debug, Default)]
pub struct BodyInfo {
    pub blob: String,
    pub text: String,
    pub snippet: String,
    pub has_attachments: bool,
}

#[derive(Clone, Debug)]
pub struct NewMessage {
    pub account_id: AccountId,
    pub remote_id: String,
    pub mailbox_id: MailboxId,
    pub uid: Option<u32>,
    /// Received time in unix seconds; it orders the message.
    pub date: i64,
    pub envelope: Envelope,
    pub flags: Flags,
    pub size: u32,
    /// A provider thread id (X-GM-THRID, conversationId) when there is one.
    pub thread_hint: Option<String>,
    pub body: Option<BodyInfo>,
}
