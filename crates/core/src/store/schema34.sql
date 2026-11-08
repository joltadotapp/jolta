-- The automatic reply state each provider last reported, for the mail
-- list's strip (C4 U3). A Microsoft 365 account with calendar off and an
-- IMAP account have no row: Jolta can't know theirs.
CREATE TABLE auto_reply (
    account_id INTEGER PRIMARY KEY REFERENCES account(id) ON DELETE CASCADE,
    state TEXT NOT NULL,          -- off, on
    starts INTEGER, ends INTEGER, -- unix seconds; NULL is an open end
    internal TEXT NOT NULL DEFAULT '',
    external TEXT NOT NULL DEFAULT '',
    audience TEXT NOT NULL DEFAULT 'all',
    editable INTEGER NOT NULL,
    fetched INTEGER NOT NULL      -- unix milliseconds the read started
);
