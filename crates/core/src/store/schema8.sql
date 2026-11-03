-- Connection settings a power user can change per account. The defaults
-- match how Jolta connected before: TLS everywhere except plain IMAP to a
-- local test server, SMTP on 465 with TLS and elsewhere with STARTTLS.
ALTER TABLE account ADD COLUMN imap_security TEXT NOT NULL DEFAULT 'tls';
ALTER TABLE account ADD COLUMN smtp_security TEXT NOT NULL DEFAULT 'starttls';
ALTER TABLE account ADD COLUMN smtp_auth TEXT NOT NULL DEFAULT 'same';
ALTER TABLE account ADD COLUMN smtp_username TEXT NOT NULL DEFAULT '';
ALTER TABLE account ADD COLUMN verify_certs INTEGER NOT NULL DEFAULT 1;
UPDATE account SET smtp_security = 'tls' WHERE smtp_port = 465;
UPDATE account SET imap_security = 'none' WHERE imap_host IN ('127.0.0.1', 'localhost', '::1') AND imap_port <> 993;
