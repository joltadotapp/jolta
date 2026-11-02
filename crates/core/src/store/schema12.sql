-- Snippets: reusable text inserted while writing (⌘;), in the body format
-- of `rich.rs`. {first name}, {name} and {email} fill in from the first
-- recipient.
CREATE TABLE snippet (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    body TEXT NOT NULL DEFAULT ''
);
