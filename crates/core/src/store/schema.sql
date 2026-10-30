CREATE TABLE account (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    address TEXT NOT NULL,
    display_name TEXT NOT NULL DEFAULT '',
    username TEXT NOT NULL,
    imap_host TEXT NOT NULL DEFAULT '',
    imap_port INTEGER NOT NULL DEFAULT 993,
    smtp_host TEXT NOT NULL DEFAULT '',
    smtp_port INTEGER NOT NULL DEFAULT 465,
    -- Keychain account name for this account's secrets; unique across data folders.
    secret_ref TEXT NOT NULL UNIQUE,
    created INTEGER NOT NULL
);

CREATE TABLE mailbox (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    remote_id TEXT NOT NULL,
    name TEXT NOT NULL,
    role TEXT,
    delimiter TEXT,
    selectable INTEGER NOT NULL DEFAULT 1,
    uidvalidity INTEGER,
    highest_modseq INTEGER,
    delta_link TEXT,
    UNIQUE (account_id, remote_id)
);

CREATE TABLE thread (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES account(id) ON DELETE CASCADE
);

-- Every Message-ID seen in a thread (including ones only referenced), plus
-- provider thread ids such as X-GM-THRID, maps to its thread.
CREATE TABLE thread_key (
    account_id INTEGER NOT NULL,
    key TEXT NOT NULL,
    thread_id INTEGER NOT NULL,
    PRIMARY KEY (account_id, key)
) WITHOUT ROWID;
CREATE INDEX thread_key_thread ON thread_key (thread_id);

-- id is (received unix seconds << 20) | counter, so id order is date order
-- and the FTS index can walk newest-first by rowid.
CREATE TABLE message (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    remote_id TEXT NOT NULL,
    thread_id INTEGER NOT NULL,
    message_id TEXT,
    date INTEGER NOT NULL,
    from_name TEXT NOT NULL DEFAULT '',
    from_addr TEXT NOT NULL DEFAULT '',
    to_addrs TEXT NOT NULL DEFAULT '',
    cc_addrs TEXT NOT NULL DEFAULT '',
    subject TEXT NOT NULL DEFAULT '',
    snippet TEXT NOT NULL DEFAULT '',
    flags INTEGER NOT NULL DEFAULT 0,
    has_attachments INTEGER NOT NULL DEFAULT 0,
    size INTEGER NOT NULL DEFAULT 0,
    blob TEXT,
    list_id TEXT,
    UNIQUE (account_id, remote_id)
);
CREATE INDEX message_thread ON message (thread_id, id);
CREATE INDEX message_msgid ON message (account_id, message_id);
CREATE INDEX message_no_blob ON message (account_id, id) WHERE blob IS NULL;

CREATE TABLE message_mailbox (
    mailbox_id INTEGER NOT NULL REFERENCES mailbox(id) ON DELETE CASCADE,
    message_id INTEGER NOT NULL REFERENCES message(id) ON DELETE CASCADE,
    uid INTEGER,
    PRIMARY KEY (mailbox_id, message_id)
) WITHOUT ROWID;
CREATE INDEX message_mailbox_message ON message_mailbox (message_id);
CREATE INDEX message_mailbox_uid ON message_mailbox (mailbox_id, uid);

CREATE VIRTUAL TABLE message_fts USING fts5 (
    subject, sender, recipients, body,
    content = '',
    contentless_delete = 1,
    tokenize = 'unicode61 remove_diacritics 2',
    prefix = '2 3'
);
