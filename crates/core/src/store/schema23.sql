-- The version of the event or occurrence an answer from Jolta went to, as
-- the JSON of its SEQUENCE, span and repeat rule: the card and the list
-- both judge by it whether the answer holds for a later version, with or
-- without a reply mail. Answers kept before have none and hold.
-- `invitation.timing` now places a single event too, so the list can judge
-- its answer; rows stored before get it from one more pass of the scan.
ALTER TABLE invitation_answer ADD COLUMN version TEXT;
INSERT OR IGNORE INTO invitation_scan SELECT DISTINCT message_id FROM invitation;
