/* Derived from subversion/libsvn_wc/wc-metadata.sql at tag 1.14.5 (Apache Subversion,
 * Licensed under the Apache License, Version 2.0; see the NOTICE file of that project).
 * Statements are unchanged apart from removing the STMT markers and the user_version
 * define block, which OpenSVN sets from Rust (format 31). */

ANALYZE sqlite_master; /* Creates empty sqlite_stat1 if necessary */

DELETE FROM sqlite_stat1
WHERE tbl in ('NODES', 'ACTUAL_NODE', 'LOCK', 'WC_LOCK', 'EXTERNALS');

INSERT INTO sqlite_stat1(tbl, idx, stat) VALUES
    ('NODES', 'sqlite_autoindex_NODES_1',               '8000 8000 2 1');
INSERT INTO sqlite_stat1(tbl, idx, stat) VALUES
    ('NODES', 'I_NODES_PARENT',                         '8000 8000 10 2 1');
/* Tell a lie: We ignore that 99.9% of all moved_to values are NULL */
INSERT INTO sqlite_stat1(tbl, idx, stat) VALUES
    ('NODES', 'I_NODES_MOVED',                          '8000 8000 1 1');

INSERT INTO sqlite_stat1(tbl, idx, stat) VALUES
    ('ACTUAL_NODE', 'sqlite_autoindex_ACTUAL_NODE_1',   '8000 8000 1');
INSERT INTO sqlite_stat1(tbl, idx, stat) VALUES
    ('ACTUAL_NODE', 'I_ACTUAL_PARENT',                  '8000 8000 10 1');

INSERT INTO sqlite_stat1(tbl, idx, stat) VALUES
    ('LOCK', 'sqlite_autoindex_LOCK_1',                 '100 100 1');

INSERT INTO sqlite_stat1(tbl, idx, stat) VALUES
    ('WC_LOCK', 'sqlite_autoindex_WC_LOCK_1',           '100 100 1');

INSERT INTO sqlite_stat1(tbl, idx, stat) VALUES
    ('EXTERNALS','sqlite_autoindex_EXTERNALS_1',        '100 100 1');
INSERT INTO sqlite_stat1(tbl, idx, stat) VALUES
    ('EXTERNALS','I_EXTERNALS_DEFINED',                 '100 100 3 1');

/* sqlite_autoindex_WORK_QUEUE_1 doesn't exist because WORK_QUEUE is
   a INTEGER PRIMARY KEY AUTOINCREMENT table */

ANALYZE sqlite_master; /* Loads sqlite_stat1 data for query optimizer */
/* ------------------------------------------------------------------------- */

