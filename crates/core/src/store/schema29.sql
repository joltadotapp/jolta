-- Edits to calendars, applied to the store at once and sent by the
-- calendar worker. `base` is the event as the user started editing it,
-- the left side of the three-way merge on a 412; NULL for a create.
-- `target` is the event as the user saved it, or for a move the
-- destination calendar id. `remote_key` makes a retried create land on
-- the same object: Graph's transactionId, Google's event id, the CalDAV
-- resource name. Ops that email people wait until `send_after`.
CREATE TABLE calendar_op (
    id INTEGER PRIMARY KEY,
    calendar_id INTEGER NOT NULL REFERENCES calendar(id) ON DELETE CASCADE,
    uid TEXT NOT NULL,
    recurrence_id TEXT NOT NULL DEFAULT '',
    kind TEXT NOT NULL,
    scope TEXT NOT NULL DEFAULT 'all',
    notify TEXT NOT NULL DEFAULT 'none',
    message TEXT,
    base TEXT,
    target TEXT NOT NULL,
    remote_key TEXT NOT NULL,
    send_after INTEGER NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending',
    tried INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    lost TEXT,
    created INTEGER NOT NULL
);
CREATE INDEX calendar_op_pending ON calendar_op(send_after) WHERE state = 'pending';
CREATE INDEX calendar_op_event ON calendar_op(calendar_id, uid);

-- The account a local or scheduling-less calendar sends invitations from.
ALTER TABLE calendar ADD COLUMN send_from INTEGER REFERENCES account(id) ON DELETE SET NULL;
