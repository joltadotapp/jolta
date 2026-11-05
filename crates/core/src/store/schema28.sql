-- Graph exceptions now carry their recurrence id: the delta round selects
-- originalStart, which delta leaves out unless asked. Before, a moved
-- occurrence was stored with an empty recurrence id, and the series hid it.
-- Graph calendars start a new round, and their events lose their ETags so
-- that round rewrites the ones it sends unchanged.
UPDATE event SET etag = NULL WHERE calendar_id IN (SELECT id FROM calendar WHERE source = 'graph');
UPDATE calendar SET cursor = NULL, synced = NULL WHERE source = 'graph';
