-- A checkout in progress, written before the engine starts and removed once it finishes. A row
-- left behind names a checkout that was interrupted (connection lost, cancelled, app closed or
-- killed): start-up registers its working copy, and "Continue" finishes it with the choices the
-- user made. The target revision and depths are read from the working copy itself (wc.db).
CREATE TABLE checkout_journal (
  root_path        TEXT PRIMARY KEY NOT NULL,
  url              TEXT NOT NULL,
  depth            TEXT NOT NULL,
  ignore_externals INTEGER NOT NULL DEFAULT 0 CHECK (ignore_externals IN (0, 1)),
  started_at       TEXT NOT NULL
);
