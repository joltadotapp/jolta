-- Newsletters often carry List-Unsubscribe without List-Id.
ALTER TABLE message ADD COLUMN list_unsubscribe TEXT;

-- Bodies stored before Jolta read list headers from them. The engine works
-- through this list once at start and empties it.
CREATE TABLE header_scan (message_id INTEGER PRIMARY KEY);
INSERT INTO header_scan SELECT id FROM message WHERE blob IS NOT NULL AND list_id IS NULL;

-- Folders the user keeps out of the sidebar. They still sync and search.
ALTER TABLE mailbox ADD COLUMN hidden INTEGER NOT NULL DEFAULT 0;
