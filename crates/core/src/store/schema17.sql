-- Calendar sync (docs/calendar-plan.md, phases C0 and C1).

-- A CalDAV address typed under Advanced, for servers discovery can't find.
ALTER TABLE account ADD COLUMN caldav_url TEXT;

-- The user's own color wins over the server's, which a re-list refreshes.
ALTER TABLE calendar ADD COLUMN user_color TEXT;
-- Seconds between polls when the source says (a feed's REFRESH-INTERVAL).
ALTER TABLE calendar ADD COLUMN refresh INTEGER;

-- What the week view shows beside the title, so a page of occurrences
-- needs no JSON.
ALTER TABLE event ADD COLUMN location TEXT NOT NULL DEFAULT '';
ALTER TABLE event ADD COLUMN show_as TEXT NOT NULL DEFAULT 'busy';
ALTER TABLE event ADD COLUMN meeting_provider TEXT;
ALTER TABLE event ADD COLUMN join_url TEXT;
ALTER TABLE event ADD COLUMN has_attendees INTEGER NOT NULL DEFAULT 0;

-- Deleting an event looks up its occurrences through each foreign key.
CREATE INDEX occurrence_event ON occurrence(event_id);
-- A range query bounds the start from below by the longest occurrence, read
-- from these in one index step.
CREATE INDEX occurrence_utc_length ON occurrence(end_utc - start_utc) WHERE start_utc IS NOT NULL;
CREATE INDEX occurrence_day_length ON occurrence(end_day - start_day) WHERE start_day IS NOT NULL;
