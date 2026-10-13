-- cache.sqlite schema, docs/system-design/03 section 6.2. The whole file can be deleted at any time.
CREATE TABLE log_entry (
  repo_uuid TEXT, rev INTEGER, author TEXT, date TEXT, message TEXT,
  PRIMARY KEY (repo_uuid, rev)
);

CREATE TABLE log_path (
  repo_uuid TEXT, rev INTEGER, path TEXT, action TEXT, kind TEXT,
  copy_from_path TEXT, copy_from_rev INTEGER
);
CREATE INDEX log_path_by_path ON log_path (repo_uuid, path, rev DESC);

CREATE TABLE log_coverage (repo_uuid TEXT, path TEXT, from_rev INTEGER, to_rev INTEGER);

CREATE TABLE status_snapshot (
  wc_id TEXT, path TEXT, data BLOB,
  PRIMARY KEY (wc_id, path)
);

CREATE TABLE blob_cache (
  repo_uuid TEXT, path TEXT, rev INTEGER, sha1 TEXT, size INTEGER, last_access TEXT,
  PRIMARY KEY (repo_uuid, path, rev)
);
