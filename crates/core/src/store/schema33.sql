-- Events keep what Google's status events say: out of office and focus
-- time with what they decline, and working locations (C4 U1). Google
-- calendars start a new round, and their events lose their ETags so that
-- round rewrites the ones it sends unchanged.
UPDATE event SET etag = NULL WHERE calendar_id IN (SELECT id FROM calendar WHERE source = 'google');
UPDATE calendar SET cursor = NULL, synced = NULL WHERE source = 'google';
