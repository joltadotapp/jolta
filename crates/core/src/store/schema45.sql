-- An agent is told apart by the token Jolta gives it when the user adds or
-- pairs it in Settings › Agents; only the token's SHA-256 is kept. The name
-- is a label. An agent from before has no token: it gets nothing until the
-- user pairs it again, and keeps its audit log.
ALTER TABLE agent ADD COLUMN token_hash TEXT;
CREATE UNIQUE INDEX agent_token ON agent (token_hash);
