-- A mail op's id is its Undo token, an outbox row's its Undo send token
-- and the reply an invitation answer remembers, a draft's how the app and
-- agents open and save it. Without AUTOINCREMENT, SQLite gives a deleted
-- row's id to the next row, and a stale token acts on that newer row. Each
-- table is rebuilt with AUTOINCREMENT, keeping its rows and ids, and starts
-- counting above any id still remembered elsewhere.

CREATE TABLE op_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
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
    created INTEGER NOT NULL,
    tried INTEGER NOT NULL DEFAULT 0
);
INSERT INTO op_new (id, account_id, kind, messages, from_mailbox, to_mailbox, add_flags, remove_flags, state, attempts, error, created, tried)
    SELECT id, account_id, kind, messages, from_mailbox, to_mailbox, add_flags, remove_flags, state, attempts, error, created, tried FROM op;
DROP TABLE op;
ALTER TABLE op_new RENAME TO op;
CREATE INDEX op_pending ON op (account_id, id) WHERE state = 'pending';

CREATE TABLE draft_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER REFERENCES account(id) ON DELETE CASCADE,
    to_addrs TEXT NOT NULL DEFAULT '',
    cc_addrs TEXT NOT NULL DEFAULT '',
    bcc_addrs TEXT NOT NULL DEFAULT '',
    subject TEXT NOT NULL DEFAULT '',
    body TEXT NOT NULL DEFAULT '',
    in_reply_to TEXT,
    refs TEXT NOT NULL DEFAULT '',
    replying_to INTEGER,
    -- One attachment per line: name, MIME type and path, tab-separated.
    attachments TEXT NOT NULL DEFAULT '',
    server_message_id TEXT,
    state TEXT NOT NULL DEFAULT 'editing',
    dirty INTEGER NOT NULL DEFAULT 0,
    updated INTEGER NOT NULL
);
INSERT INTO draft_new (id, account_id, to_addrs, cc_addrs, bcc_addrs, subject, body, in_reply_to, refs, replying_to, attachments, server_message_id, state, dirty, updated)
    SELECT id, account_id, to_addrs, cc_addrs, bcc_addrs, subject, body, in_reply_to, refs, replying_to, attachments, server_message_id, state, dirty, updated FROM draft;
DROP TABLE draft;
ALTER TABLE draft_new RENAME TO draft;

CREATE TABLE outbox_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    draft_id INTEGER,
    blob TEXT NOT NULL,
    message_id TEXT NOT NULL,
    sender TEXT NOT NULL,
    recipients TEXT NOT NULL,
    replying_to INTEGER,
    send_after INTEGER NOT NULL,
    state TEXT NOT NULL DEFAULT 'queued',
    attempts INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    created INTEGER NOT NULL
);
INSERT INTO outbox_new (id, account_id, draft_id, blob, message_id, sender, recipients, replying_to, send_after, state, attempts, error, created)
    SELECT id, account_id, draft_id, blob, message_id, sender, recipients, replying_to, send_after, state, attempts, error, created FROM outbox;
DROP TABLE outbox;
ALTER TABLE outbox_new RENAME TO outbox;
CREATE INDEX outbox_queued ON outbox (account_id, send_after) WHERE state = 'queued';

-- A sent message's outbox row keeps the id of its deleted draft, and an
-- answer keeps the id of a reply mail taken back.
INSERT INTO sqlite_sequence (name, seq) SELECT t, 0 FROM (SELECT 'draft' AS t UNION ALL SELECT 'outbox')
    WHERE t NOT IN (SELECT name FROM sqlite_sequence);
UPDATE sqlite_sequence SET seq = max(seq, (SELECT coalesce(max(draft_id), 0) FROM outbox)) WHERE name = 'draft';
UPDATE sqlite_sequence SET seq = max(seq, (SELECT coalesce(max(reply), 0) FROM invitation_answer)) WHERE name = 'outbox';
