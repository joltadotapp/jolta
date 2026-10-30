-- A snoozed thread sleeps in the account's Snoozed mailbox until wake_at.
CREATE TABLE snooze (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    thread_id INTEGER NOT NULL,
    wake_at INTEGER NOT NULL,
    state TEXT NOT NULL DEFAULT 'sleeping'
);
CREATE INDEX snooze_due ON snooze (account_id, wake_at) WHERE state = 'sleeping';

-- Split inbox: each section is a saved query shown as a tab over the inbox.
CREATE TABLE section (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    position INTEGER NOT NULL,
    query TEXT NOT NULL
);
INSERT INTO section (name, position, query) VALUES
    ('VIP', 1, 'is:vip'),
    ('Newsletters', 2, 'is:newsletter'),
    ('Notifications', 3, 'is:notification');

-- Senders marked as VIP (is:vip).
CREATE TABLE vip (address TEXT PRIMARY KEY COLLATE NOCASE) WITHOUT ROWID;
