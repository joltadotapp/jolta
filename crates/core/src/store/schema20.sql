-- The events that calendar parts in mail name: invitations, replies,
-- cancellations. One row per VEVENT; `recurrence_id` is '' for a single
-- event or a whole series, written as in `event`. A message reaches its
-- event in the user's calendars through `uid`, so event rows that a sync
-- replaces need no update here.
CREATE TABLE invitation (
    message_id INTEGER NOT NULL REFERENCES message(id) ON DELETE CASCADE,
    uid TEXT NOT NULL,
    recurrence_id TEXT NOT NULL DEFAULT '',
    sequence INTEGER NOT NULL DEFAULT 0,
    method TEXT NOT NULL,
    organizer TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (message_id, uid, recurrence_id)
);
CREATE INDEX invitation_uid ON invitation(uid);
-- An invitation looks its event up by UID across every calendar.
CREATE INDEX event_by_uid ON event(uid, recurrence_id);
-- "You haven't written to this sender" asks whether the user answered mail
-- from an address; few messages carry the answered flag (4), so this stays small.
CREATE INDEX message_answered ON message(from_addr COLLATE NOCASE) WHERE (flags & 4) != 0;

-- Bodies stored before Jolta read invitations get one background pass, for
-- mail of the last 400 days: the rows serve upcoming events, and an older
-- invitation still shows its card, which the reader makes from the message
-- itself. The attachment flag can't narrow it further: 840 of 985 stored
-- invitations measured had none.
CREATE TABLE invitation_scan (message_id INTEGER PRIMARY KEY);
INSERT INTO invitation_scan SELECT id FROM message
WHERE blob IS NOT NULL AND blob NOT LIKE 'synth:%' AND date > CAST(strftime('%s', 'now') AS INTEGER) - 400 * 86400;
