-- A copy of an invitation the user put in a calendar with Add to calendar,
-- because the calendars of the account the invitation came to hold none (its
-- calendar is off in Jolta, as for iCloud). The copy keeps the invitation's
-- UID on every provider; `remote_id` names the object Jolta created there.
-- `organizer` is the meeting's organizer the copy was added from: only mail
-- from that address changes or removes the copy, whatever other mail remains.
-- `written` is the JSON list of the versions written into the copy, each a
-- recurrence id ('' for the series or single event), SEQUENCE and DTSTAMP, so
-- the card offers Update in your calendar for a newer one.
CREATE TABLE added_copy (
    account_id INTEGER NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    uid TEXT NOT NULL,
    calendar_id INTEGER NOT NULL REFERENCES calendar(id) ON DELETE CASCADE,
    remote_id TEXT NOT NULL,
    organizer TEXT NOT NULL,
    written TEXT NOT NULL,
    PRIMARY KEY (account_id, uid)
);
