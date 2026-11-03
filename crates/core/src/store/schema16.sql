-- Calendars (docs/calendar-plan.md). Calendar is off per account until the
-- user turns it on; local, webcal and EventKit calendars have no account.
ALTER TABLE account ADD COLUMN calendar INTEGER NOT NULL DEFAULT 0;

CREATE TABLE calendar (
    id INTEGER PRIMARY KEY,
    account_id INTEGER REFERENCES account(id) ON DELETE CASCADE,
    source TEXT NOT NULL,
    remote_id TEXT NOT NULL,
    name TEXT NOT NULL,
    color TEXT,
    zone TEXT,
    access TEXT NOT NULL DEFAULT 'read',
    shown INTEGER NOT NULL DEFAULT 0,
    counts_as_busy INTEGER NOT NULL DEFAULT 1,
    notify INTEGER NOT NULL DEFAULT 1,
    default_reminder INTEGER,
    is_default INTEGER NOT NULL DEFAULT 0,
    cursor TEXT,
    synced INTEGER
);
CREATE UNIQUE INDEX calendar_remote ON calendar(ifnull(account_id, 0), source, remote_id);

-- One row per single event, series master or override. `data` is the
-- EventData as JSON; the columns beside it are what lists and searches read.
-- `recurrence_id` is '' for singles and masters.
CREATE TABLE event (
    id INTEGER PRIMARY KEY,
    calendar_id INTEGER NOT NULL REFERENCES calendar(id) ON DELETE CASCADE,
    remote_id TEXT NOT NULL,
    etag TEXT,
    uid TEXT NOT NULL,
    recurrence_id TEXT NOT NULL DEFAULT '',
    data TEXT NOT NULL,
    title TEXT NOT NULL,
    status TEXT NOT NULL,
    my_response TEXT,
    sequence INTEGER NOT NULL DEFAULT 0,
    blob TEXT,
    changed_locally INTEGER NOT NULL DEFAULT 0,
    UNIQUE (calendar_id, remote_id, recurrence_id)
);
CREATE INDEX event_uid ON event(calendar_id, uid);

-- Expanded instances for the view window, rebuilt per series when it
-- changes. Timed occurrences use UTC seconds, all-day ones days since
-- 1970-01-01 with an exclusive end; exactly one pair is set.
CREATE TABLE occurrence (
    calendar_id INTEGER NOT NULL REFERENCES calendar(id) ON DELETE CASCADE,
    series_id INTEGER NOT NULL REFERENCES event(id) ON DELETE CASCADE,
    event_id INTEGER NOT NULL REFERENCES event(id) ON DELETE CASCADE,
    start_utc INTEGER,
    end_utc INTEGER,
    start_day INTEGER,
    end_day INTEGER,
    busy INTEGER NOT NULL,
    CHECK ((start_utc IS NULL) = (end_utc IS NULL) AND (start_day IS NULL) = (end_day IS NULL) AND (start_utc IS NULL) <> (start_day IS NULL))
);
CREATE INDEX occurrence_utc ON occurrence(start_utc) WHERE start_utc IS NOT NULL;
CREATE INDEX occurrence_day ON occurrence(start_day) WHERE start_day IS NOT NULL;
CREATE INDEX occurrence_series ON occurrence(series_id);
