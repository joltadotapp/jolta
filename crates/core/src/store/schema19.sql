-- Bodies stored before Jolta read S/MIME counted the signature file as an
-- attachment and indexed an opaque-signed message without its text. A
-- background scan parses the ones that are S/MIME again; only messages
-- marked with attachments can be affected.
CREATE TABLE smime_scan (message_id INTEGER PRIMARY KEY);
INSERT INTO smime_scan SELECT id FROM message WHERE blob IS NOT NULL AND blob NOT LIKE 'synth:%' AND has_attachments = 1;
