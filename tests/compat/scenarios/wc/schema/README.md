# wc schema compat scenarios

Executable checks live in `crates/svn-wc/tests/` (each creates its own working copies with the local `svn`):

| Scenario | Test |
|---|---|
| OpenSVN-created wc.db structurally equals svn 1.15.0 checkout | `schema_structure::created_schema_matches_svn_checkout` |
| ... equals svn 1.14.5 checkout (opt-in: Docker `alpine:3.22`, `OPENSVN_TEST_DOCKER_SVN=1`) | `schema_structure::created_schema_matches_svn_1_14_5_in_docker` |
| svn add/commit/update/cleanup work on an OpenSVN-created WC | `schema_open::working_copy_created_by_opensvn_is_accepted_by_svn` |
| format 31 rw, 32 ro (`svn checkout --compatible-version=1.15`), <31 and >32 rejected | `schema_open::*format*` |
| skel codec vs blobs from real wc.db | `skel_real_blobs` |
