-- app.sqlite schema, docs/system-design/03 section 6.1.
-- trusted_cert is keyed by host + port + sha256: a certificate accepted for one port of a host
-- must not be trusted on another service of the same host.
CREATE TABLE working_copy (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  root_path TEXT NOT NULL UNIQUE,
  repo_uuid TEXT NOT NULL,
  repo_root_url TEXT NOT NULL,
  sort_order INTEGER,
  added_at TEXT NOT NULL,
  last_opened_at TEXT
);

CREATE TABLE repo_layout (
  repo_uuid TEXT PRIMARY KEY,
  mode TEXT NOT NULL CHECK (mode IN ('auto', 'custom', 'none')),
  trunk TEXT,
  branches TEXT,
  tags TEXT
);

CREATE TABLE recent_url (
  url TEXT PRIMARY KEY,
  last_used_at TEXT NOT NULL
);

CREATE TABLE trusted_cert (
  host TEXT NOT NULL,
  port INTEGER NOT NULL,
  sha256 TEXT NOT NULL,
  subject TEXT,
  expires_at TEXT,
  trusted_at TEXT NOT NULL,
  PRIMARY KEY (host, port, sha256)
);

-- Metadata only, never a password. Keyed by origin, not by realm alone.
CREATE TABLE account (
  scheme TEXT NOT NULL,
  host TEXT NOT NULL,
  port INTEGER NOT NULL,
  realm TEXT NOT NULL,
  username TEXT,
  method TEXT NOT NULL CHECK (method IN ('basic', 'negotiate')),
  last_used_at TEXT,
  PRIMARY KEY (scheme, host, port, realm)
);

-- Who holds WC_LOCK (wc.db has no owner column).
CREATE TABLE wc_lock_owner (
  wc_root TEXT NOT NULL,
  dir TEXT NOT NULL,
  pid INTEGER NOT NULL,
  boot_id TEXT NOT NULL,
  acquired_at TEXT NOT NULL,
  PRIMARY KEY (wc_root, dir)
);

CREATE TABLE commit_message_history (
  id INTEGER PRIMARY KEY,
  repo_uuid TEXT,
  message TEXT NOT NULL,
  used_at TEXT NOT NULL
);

-- value is JSON.
CREATE TABLE setting (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
