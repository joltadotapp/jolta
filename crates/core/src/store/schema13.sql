-- AI summaries, one per conversation, with the newest message they cover
-- so a reply can update the summary instead of rereading everything.
CREATE TABLE ai_summary (
    thread_id INTEGER PRIMARY KEY REFERENCES thread(id) ON DELETE CASCADE,
    last_message INTEGER NOT NULL,
    model TEXT NOT NULL,
    text TEXT NOT NULL,
    at INTEGER NOT NULL
);
