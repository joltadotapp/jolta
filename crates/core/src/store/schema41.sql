-- The provider's permanent id for the user who signed in: Google's `sub`,
-- Microsoft's `oid`. An address can change, as when a Workspace or Entra
-- admin renames a user, so a new sign-in finds its account by this id.
-- Accounts added before this get it on their next sign-in.
ALTER TABLE account ADD COLUMN oauth_user_id TEXT;
CREATE UNIQUE INDEX account_oauth_user ON account(kind, oauth_user_id);
