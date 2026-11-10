-- Everyone the user has sent mail to (To and Cc of mail from one of the
-- user's addresses), for is:important: mail sent To the user by one of
-- them is important. Filled as messages are stored, and from all stored
-- mail each time the engine starts.
CREATE TABLE correspondent (
    address TEXT PRIMARY KEY COLLATE NOCASE
) WITHOUT ROWID;
