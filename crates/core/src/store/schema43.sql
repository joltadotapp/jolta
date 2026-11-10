-- Other addresses an account sends from. A provider alias came from
-- Gmail's Send mail as or Microsoft 365's proxy addresses and is replaced
-- on each fetch; hidden keeps one the user removed from coming back.
CREATE TABLE account_alias (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id INTEGER NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    address TEXT NOT NULL,
    display_name TEXT NOT NULL DEFAULT '',
    source TEXT NOT NULL CHECK (source IN ('manual', 'provider')),
    hidden INTEGER NOT NULL DEFAULT 0,
    UNIQUE (account_id, address COLLATE NOCASE)
);

-- Every address that is the user: each account's own and its aliases.
CREATE VIEW own_address AS
    SELECT id AS account_id, address FROM account
    UNION ALL
    SELECT account_id, address FROM account_alias WHERE NOT hidden;

-- The alias a draft is written from; NULL is the account's own address.
ALTER TABLE draft ADD COLUMN from_addr TEXT;
