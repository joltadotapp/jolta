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

pub(crate) use id_type;

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

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                let text = <std::borrow::Cow<'de, str>>::deserialize(d)?;
                text.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}

pub(crate) use text_enum;

text_enum!(AccountKind {
    Imap => "imap",
    Gmail => "gmail",
    Graph => "graph",
});

impl AccountKind {
    /// Gmail files a message under any number of labels. IMAP folders and
    /// Microsoft 365's hold a message in one place, so there Jolta only moves.
    pub fn has_labels(self) -> bool {
        self == AccountKind::Gmail
    }
}

// How a connection is protected: TLS from the first byte, an upgrade with
// STARTTLS, or none (only sensible for a server on this machine).
text_enum!(Security {
    Tls => "tls",
    StartTls => "starttls",
    Plain => "none",
});

// Which login SMTP uses: the IMAP one, its own, or none.
text_enum!(SmtpAuth {
    SameAsIncoming => "same",
    Separate => "separate",
    NoAuth => "none",
});

// `System` marks folders the server manages for itself (Gmail's Starred and
// Important, Outlook's Outbox and Conversation History). Jolta keeps them out
// of the sidebar.
text_enum!(MailboxRole {
    Inbox => "inbox",
    Sent => "sent",
    Drafts => "drafts",
    Archive => "archive",
    Trash => "trash",
    Spam => "spam",
    Snoozed => "snoozed",
    All => "all",
    System => "system",
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

#[derive(Clone, Debug, PartialEq, Eq)]
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
    pub imap_security: Security,
    pub smtp_security: Security,
    pub smtp_auth: SmtpAuth,
    /// The SMTP login when `smtp_auth` is `Separate`.
    pub smtp_username: String,
    /// Off accepts any certificate, for self-signed servers.
    pub verify_certs: bool,
    /// Added below new messages, replies and forwards; empty for none.
    pub signature: String,
    /// Calendar is on. For Google and Microsoft this also means the user
    /// granted calendar access, so token refreshes ask for it.
    pub calendar: bool,
}

/// What the account settings screen can change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountSettings {
    pub display_name: String,
    pub address: String,
    pub username: String,
    pub imap_host: String,
    pub imap_port: u16,
    pub imap_security: Security,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: Security,
    pub smtp_auth: SmtpAuth,
    pub smtp_username: String,
    pub verify_certs: bool,
}

impl Account {
    pub fn settings(&self) -> AccountSettings {
        AccountSettings {
            display_name: self.display_name.clone(),
            address: self.address.clone(),
            username: self.username.clone(),
            imap_host: self.imap_host.clone(),
            imap_port: self.imap_port,
            imap_security: self.imap_security,
            smtp_host: self.smtp_host.clone(),
            smtp_port: self.smtp_port,
            smtp_security: self.smtp_security,
            smtp_auth: self.smtp_auth,
            smtp_username: self.smtp_username.clone(),
            verify_certs: self.verify_certs,
        }
    }

    pub fn with_settings(&self, s: AccountSettings) -> Account {
        Account {
            display_name: s.display_name,
            address: s.address,
            username: s.username,
            imap_host: s.imap_host,
            imap_port: s.imap_port,
            imap_security: s.imap_security,
            smtp_host: s.smtp_host,
            smtp_port: s.smtp_port,
            smtp_security: s.smtp_security,
            smtp_auth: s.smtp_auth,
            smtp_username: s.smtp_username,
            verify_certs: s.verify_certs,
            ..self.clone()
        }
    }

    /// Who hosts the account, for display: "iCloud", "Fastmail", "Google".
    pub fn provider_label(&self) -> &'static str {
        let host = self.imap_host.to_ascii_lowercase();
        match self.kind {
            AccountKind::Gmail => "Google",
            AccountKind::Graph => "Microsoft 365",
            AccountKind::Imap if host.is_empty() => "Synthetic",
            AccountKind::Imap if host.ends_with("mail.me.com") => "iCloud",
            AccountKind::Imap if host.contains("fastmail") => "Fastmail",
            AccountKind::Imap if host.ends_with("gmail.com") => "Google",
            AccountKind::Imap if host == "127.0.0.1" || host == "localhost" => "Local",
            AccountKind::Imap => "IMAP",
        }
    }
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
    pub hidden: bool,
    /// Whether a sync has gone through this mailbox once, so an empty list
    /// means empty rather than not downloaded yet.
    pub synced: bool,
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
    pub list_unsubscribe: Option<String>,
    pub has_attachments: bool,
}

/// What a downloaded body adds to a message.
#[derive(Clone, Debug, Default)]
pub struct BodyInfo {
    pub blob: String,
    pub text: String,
    pub snippet: String,
    pub has_attachments: bool,
    /// List headers, which a Graph listing doesn't carry: only the full
    /// message has them.
    pub list_id: Option<String>,
    pub list_unsubscribe: Option<String>,
    /// The events its calendar parts name.
    pub invitations: Vec<crate::calendar::invitation::Invitation>,
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
