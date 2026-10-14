-- A saved password the server rejected (401) is marked stale instead of being deleted: it stays
-- in the OS credential store but is no longer offered automatically, also after a restart,
-- until the user enters a password for the account again.
ALTER TABLE account ADD COLUMN stale INTEGER NOT NULL DEFAULT 0 CHECK (stale IN (0, 1));
