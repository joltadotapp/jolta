-- Rooms on Microsoft 365 (C4 U5). `account.rooms` is the answer to the
-- room lists consent (Place.Read.All): NULL never asked, 'granted', else
-- the reason Microsoft gave for refusing. `room` keeps the organization's
-- rooms as the room lists last said, and `account.rooms_fetched` when they
-- said it (unix seconds), even when they listed none; rooms from the user's
-- own meetings come from the stored events instead.
ALTER TABLE account ADD COLUMN rooms TEXT;
ALTER TABLE account ADD COLUMN rooms_fetched INTEGER;
CREATE TABLE room (
    account_id INTEGER NOT NULL REFERENCES account(id) ON DELETE CASCADE,
    address TEXT NOT NULL,
    name TEXT NOT NULL,
    capacity INTEGER,
    building TEXT,
    floor TEXT,
    list TEXT,
    accessible INTEGER NOT NULL DEFAULT 0,
    video TEXT,
    display TEXT,
    PRIMARY KEY (account_id, address)
);
