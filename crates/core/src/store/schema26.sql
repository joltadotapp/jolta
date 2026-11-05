-- Replies to the user's own meetings that File responses automatically
-- settled, archived or left in the inbox for good, one row per account and
-- Message-ID. A reply is settled once: the user moving it back to the inbox,
-- or a server handing out the same mail under a new id, never files it again.
CREATE TABLE settled_reply (
    account_id INTEGER NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    message_id TEXT NOT NULL,
    PRIMARY KEY (account_id, message_id)
);
