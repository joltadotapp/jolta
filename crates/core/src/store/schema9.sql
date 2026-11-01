-- Addresses removed from compose suggestions, and when. Mail sent to one
-- after that brings it back.
CREATE TABLE forgotten_address (address TEXT PRIMARY KEY COLLATE NOCASE, at INTEGER NOT NULL) WITHOUT ROWID;
