-- A switch in progress, written before the engine starts and removed once it finishes. A row
-- left behind names a switch that stopped half-way (connection lost for good, cancelled, app
-- closed or killed): the working copy then points partly to the new URL. "Continue" finishes
-- the switch to this URL, never a plain update, which would keep the folders the switch had not
-- reached at their old URL. The revision is read from the working copy (its incomplete folders).
CREATE TABLE switch_journal (
  root_path  TEXT PRIMARY KEY NOT NULL,
  url        TEXT NOT NULL,
  started_at TEXT NOT NULL
);
