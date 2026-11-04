-- The answer the user gave an invitation from Jolta, kept per account and
-- event rather than per message: an update that arrives later shows it
-- too, and storing a message's body again, which rewrites its `invitation`
-- rows, keeps it. The calendar's copy, when there is one, says more and
-- wins; this answers for invitations whose account has its calendar off.
-- `recurrence_id` is '' for a single event or a whole series, written as in
-- `event`; `answer` is `accepted`, `tentative` or `declined`.
CREATE TABLE invitation_answer (
    account_id INTEGER NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    uid TEXT NOT NULL,
    recurrence_id TEXT NOT NULL DEFAULT '',
    answer TEXT NOT NULL,
    PRIMARY KEY (account_id, uid, recurrence_id)
);
