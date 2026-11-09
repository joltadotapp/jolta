-- Holds (C4 U10): the user took back a release after a booking, so a slot
-- booked before no longer releases the others by itself. Unix seconds.
ALTER TABLE hold_set ADD COLUMN kept INTEGER;
-- Inserting into a draft makes one pending set, found by its draft.
CREATE INDEX hold_set_draft ON hold_set(draft_id) WHERE draft_id IS NOT NULL;
