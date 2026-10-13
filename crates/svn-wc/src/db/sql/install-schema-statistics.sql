/* wc-metadata.sql -- schema used in the wc-metadata SQLite database
 *     This is intended for use with SQLite 3
 *
 * ====================================================================
 *    Licensed to the Apache Software Foundation (ASF) under one
 *    or more contributor license agreements.  See the NOTICE file
 *    distributed with this work for additional information
 *    regarding copyright ownership.  The ASF licenses this file
 *    to you under the Apache License, Version 2.0 (the
 *    "License"); you may not use this file except in compliance
 *    with the License.  You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 *    Unless required by applicable law or agreed to in writing,
 *    software distributed under the License is distributed on an
 *    "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
 *    KIND, either express or implied.  See the License for the
 *    specific language governing permissions and limitations
 *    under the License.
 * ====================================================================
 */


/* Modified by OpenSVN: extracted from subversion/libsvn_wc/wc-metadata.sql at tag 1.14.5;
 * the STMT markers and the user_version define block were removed (OpenSVN sets
 * user_version = 31 from Rust). The statements themselves are unchanged. */

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

