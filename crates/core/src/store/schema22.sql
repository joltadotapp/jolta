-- When the event an invitation row names ends, in unix seconds: for a
-- series its last occurrence, 0 when the message gives no time, and a time
-- far past any date for a series without end. The list shows "Not
-- answered" and `is:invite` finds a request only while it is to come.
-- Rows stored before get it from one more pass of the invitation scan.
ALTER TABLE invitation ADD COLUMN ends INTEGER NOT NULL DEFAULT 0;
CREATE INDEX invitation_request_ends ON invitation(ends) WHERE method = 'REQUEST';
INSERT OR IGNORE INTO invitation_scan SELECT DISTINCT message_id FROM invitation;
