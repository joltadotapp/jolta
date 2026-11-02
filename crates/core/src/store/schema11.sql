-- "Remind me if no reply": the sent message (by its Message-ID header) and
-- when to check. A fired reminder puts its thread in inbox lists until the
-- thread is archived, trashed, moved or snoozed; a reply before then
-- cancels it.
CREATE TABLE reminder (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    message_id TEXT NOT NULL,
    sent_at INTEGER NOT NULL,
    remind_at INTEGER NOT NULL,
    thread_id INTEGER,
    fired INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX reminder_due ON reminder (account_id, remind_at) WHERE fired = 0;
CREATE INDEX reminder_fired ON reminder (thread_id) WHERE fired = 1;
