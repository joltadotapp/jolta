-- The zone the user picked for a person in Insert my availability (C4 U8),
-- an IANA name. `picked` is when, in unix seconds.
CREATE TABLE person_zone (address TEXT PRIMARY KEY COLLATE NOCASE, zone TEXT NOT NULL, picked INTEGER NOT NULL);
