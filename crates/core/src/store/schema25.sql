-- The new time a proposal from Jolta asked for with its answer, in Unix
-- seconds, kept once the provider or the outbox took it: the same proposal
-- again tells the organizer nothing new. NULL for an answer without one.
ALTER TABLE invitation_answer ADD COLUMN proposed_start INTEGER;
ALTER TABLE invitation_answer ADD COLUMN proposed_end INTEGER;
