-- MCP clients. An agent starts pending until Koen approves it in the app.
CREATE TABLE agent (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    state TEXT NOT NULL DEFAULT 'pending',
    first_seen INTEGER NOT NULL,
    last_seen INTEGER NOT NULL
);

-- What an approved agent may do, per account: read, organize (archive,
-- label, snooze) or draft. Sending is never granted; drafts wait for Koen.
CREATE TABLE agent_grant (
    agent_id INTEGER NOT NULL REFERENCES agent(id) ON DELETE CASCADE,
    account_id INTEGER NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    class TEXT NOT NULL,
    PRIMARY KEY (agent_id, account_id, class)
) WITHOUT ROWID;

-- Every tool call an agent made, allowed or not.
CREATE TABLE audit (
    id INTEGER PRIMARY KEY,
    agent_id INTEGER,
    tool TEXT NOT NULL,
    summary TEXT NOT NULL,
    outcome TEXT NOT NULL,
    at INTEGER NOT NULL
);

CREATE TABLE setting (key TEXT PRIMARY KEY, value TEXT NOT NULL) WITHOUT ROWID;
