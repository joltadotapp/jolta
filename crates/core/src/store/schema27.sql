-- The window start and zone each series' occurrences were placed with,
-- written in the transaction that writes those occurrences, so the two
-- never disagree. A series without a row is rebuilt on the next round,
-- which for an existing store is every series once.
CREATE TABLE series_built (
    calendar_id INTEGER NOT NULL REFERENCES calendar(id) ON DELETE CASCADE,
    uid TEXT NOT NULL,
    start INTEGER NOT NULL,
    zone TEXT NOT NULL,
    PRIMARY KEY (calendar_id, uid)
);

DELETE FROM setting WHERE key = 'calendar_occurrences';
