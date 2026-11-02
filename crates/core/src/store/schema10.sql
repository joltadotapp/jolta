-- The signature each account adds below new messages, replies and
-- forwards, in the body format of `rich.rs`.
ALTER TABLE account ADD COLUMN signature TEXT NOT NULL DEFAULT '';
