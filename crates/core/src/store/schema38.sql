-- Holds (C4 U9): the times a mail offers, each held by a tentative event
-- with Jolta's marker until someone books one. `recipients` has one
-- address per line; `created` and `sent` are unix seconds, and `sent` is
-- NULL while the mail is a draft. A slot's times are unix seconds; `state`
-- is pending, held, booked, taken, released or gone.
CREATE TABLE hold_set (
    id TEXT PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    calendar_id INTEGER NOT NULL REFERENCES calendar(id) ON DELETE CASCADE,
    draft_id INTEGER,
    message_id TEXT,
    recipients TEXT NOT NULL,
    subject TEXT NOT NULL,
    created INTEGER NOT NULL,
    sent INTEGER
);
CREATE TABLE hold (
    set_id TEXT NOT NULL REFERENCES hold_set(id) ON DELETE CASCADE,
    start INTEGER NOT NULL,
    end INTEGER NOT NULL,
    line TEXT NOT NULL,
    state TEXT NOT NULL,
    remote_id TEXT,
    PRIMARY KEY (set_id, start)
);
-- Sync finds a calendar's hold events by their set.
CREATE INDEX event_hold ON event(calendar_id, json_extract(data, '$.mark.set')) WHERE json_extract(data, '$.mark.is') = 'hold';
