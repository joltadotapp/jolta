-- How AI sorted each incoming message: a category and whether it waits for
-- the user's reply (ai/triage.rs).
CREATE TABLE ai_triage (
    message_id INTEGER PRIMARY KEY REFERENCES message(id) ON DELETE CASCADE,
    category TEXT NOT NULL,
    needs_reply INTEGER NOT NULL,
    model TEXT NOT NULL,
    at INTEGER NOT NULL
);
CREATE INDEX ai_triage_reply ON ai_triage (needs_reply) WHERE needs_reply = 1;
