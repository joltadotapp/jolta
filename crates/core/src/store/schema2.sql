-- Every local change waiting for, or already sent to, the server. Two
-- primitives cover all actions: change flags, or move between mailboxes.
CREATE TABLE op (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    kind TEXT NOT NULL,
    -- "message_id:uid" pairs; uid is empty when the server UID is unknown.
    messages TEXT NOT NULL,
    from_mailbox INTEGER,
    to_mailbox INTEGER,
    add_flags INTEGER NOT NULL DEFAULT 0,
    remove_flags INTEGER NOT NULL DEFAULT 0,
    state TEXT NOT NULL DEFAULT 'pending',
    attempts INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    created INTEGER NOT NULL
);
CREATE INDEX op_pending ON op (account_id, id) WHERE state = 'pending';
