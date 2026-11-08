-- Focus time (C4 U7). `account.focus_refused` is when Google last refused
-- a focus time event for the account (unix seconds): its edition doesn't
-- offer them, so Jolta adds a busy event without asking Google again until
-- 30 days have passed. NULL: never refused.
ALTER TABLE account ADD COLUMN focus_refused INTEGER;
