-- A thread's id is how agents name a conversation over MCP, and the app
-- keeps it in open windows and lists. Without AUTOINCREMENT, SQLite gives
-- the newest thread's id to the next thread once a merge or an account
-- removal deletes it, and a stale id acts on another conversation. The
-- table is rebuilt with AUTOINCREMENT, keeping its rows and ids, and
-- starts counting above any thread id still kept elsewhere.
CREATE TABLE thread_new (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER NOT NULL REFERENCES account(id) ON DELETE CASCADE
);
INSERT INTO thread_new (id, account_id) SELECT id, account_id FROM thread;
-- With foreign keys on, dropping thread deletes every AI summary through
-- its cascade, so the summaries are put back after.
CREATE TEMP TABLE ai_summary_kept AS SELECT thread_id, last_message, model, text, at FROM ai_summary;
DROP TABLE thread;
ALTER TABLE thread_new RENAME TO thread;
INSERT INTO ai_summary (thread_id, last_message, model, text, at) SELECT thread_id, last_message, model, text, at FROM ai_summary_kept;
DROP TABLE ai_summary_kept;

INSERT INTO sqlite_sequence (name, seq) SELECT 'thread', 0 WHERE 'thread' NOT IN (SELECT name FROM sqlite_sequence);
UPDATE sqlite_sequence SET seq = max(
    seq,
    (SELECT coalesce(max(thread_id), 0) FROM message),
    (SELECT coalesce(max(thread_id), 0) FROM thread_key),
    (SELECT coalesce(max(thread_id), 0) FROM snooze),
    (SELECT coalesce(max(thread_id), 0) FROM reminder)
) WHERE name = 'thread';

-- A message's id is its received second shifted left 20 bits plus a
-- counter within that second, one above the second's highest id. Once
-- that message was deleted, the next message received in the same second
-- got its id. Each second keeps its highest deleted id here, and new ids
-- count above it.
CREATE TABLE message_id_retired (second INTEGER PRIMARY KEY, last INTEGER NOT NULL);
CREATE TRIGGER message_id_retire AFTER DELETE ON message BEGIN
    INSERT INTO message_id_retired (second, last) VALUES (old.id >> 20, old.id)
        ON CONFLICT (second) DO UPDATE SET last = max(last, excluded.last);
END;
