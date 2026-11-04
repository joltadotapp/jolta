-- Graph events now carry the UID the invitation had (a Google meeting's
-- UID was packed inside Outlook's Global Object ID). Their calendars
-- download again so stored events get it.
DELETE FROM event WHERE calendar_id IN (SELECT id FROM calendar WHERE source = 'graph');
UPDATE calendar SET cursor = NULL, synced = NULL WHERE source = 'graph';
