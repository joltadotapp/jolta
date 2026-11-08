-- A calendar on this Mac whose meetings Jolta sends itself reads its
-- guests' iMIP replies from the mail of the account it sends from. Each
-- reply mail is read once, by Message-ID. Per guest and occurrence the
-- newest DTSTAMP read is kept, so an older reply never overwrites a newer
-- answer; `recurrence_id` is the occurrence's instant in UTC, so the same
-- occurrence named on another clock is the same row.
CREATE TABLE reply_read (
    account_id INTEGER NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    message_id TEXT NOT NULL,
    PRIMARY KEY (account_id, message_id)
);
CREATE TABLE guest_reply (
    calendar_id INTEGER NOT NULL REFERENCES calendar(id) ON DELETE CASCADE,
    uid TEXT NOT NULL,
    recurrence_id TEXT NOT NULL DEFAULT '',
    guest TEXT NOT NULL,
    stamp INTEGER NOT NULL,
    PRIMARY KEY (calendar_id, uid, recurrence_id, guest)
);
-- The outbox rows of the invitations an op mailed, comma-separated, and
-- for an op on a series Jolta mails itself, its changed occurrences as the
-- op left them (JSON), which its update carries.
ALTER TABLE calendar_op ADD COLUMN outbox TEXT;
ALTER TABLE calendar_op ADD COLUMN changed TEXT;
