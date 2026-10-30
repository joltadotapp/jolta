-- Messages being written. A draft row lives until its message is sent;
-- `dirty` asks the engine to replace the copy in the server's Drafts.
CREATE TABLE draft (
    id INTEGER PRIMARY KEY,
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

-- Undo send is a delay: a message waits here until send_after.
CREATE TABLE outbox (
    id INTEGER PRIMARY KEY,
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
CREATE INDEX outbox_queued ON outbox (account_id, send_after) WHERE state = 'queued';
