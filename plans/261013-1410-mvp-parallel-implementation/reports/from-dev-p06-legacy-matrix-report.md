# dev report: P06 legacy client matrix (PR #60) follow-up

Branch `feat/p06-compat-old-versions`. origin/main merged in (merge commit, no rebase).

## Changes
- **I1 (bake targets)**: `.github/workflows/compat.yml`. The PR/push job now passes
  `targets: certs,svn14-server,svn14-client,svn15-server,svn15-client`, because bake ignores compose
  profiles. The legacy job lists all 11 targets explicitly: `run --legacy` also runs the default
  svn14/svn15 scenarios, and the smokes need svn15. Both jobs set `*.args.SVN_MAKE_JOBS=4`.
- **Scenario DSL**: steps can set `fails_with: <text>`. Such a step passes only if the command exits
  non-zero and its stderr contains the text. It is validated: the text must be non-empty, and the
  step needs an svn actor running an svn or sh command. Unit tests cover success, a wrong error
  code and an unexpected success.
- **Old-client commit from a working copy created by 1.15** (`legacy/smoke-svn{18,19,110}.yaml`):
  the old client edits and commits from wc `c` (checked out by svn15, format 31), and all three
  WCs are updated afterwards. The a/c comparison now includes `info`. `wcdb` is left out because
  the two WCs were written by different client versions.
- **Format-32 negative case**: svn15 runs `checkout --compatible-version 1.15` (format 32).
  1.8, 1.9 and 1.10 (legacy smokes) and 1.14 (new default `harness/format32-refused-svn14.yaml`)
  must fail `status` and `update` with `E155021`. svn15 then still reads the WC.
- New non-Docker unit test `scenario_files::harness_scenarios_load`: every harness YAML (default
  and legacy) parses and validates.

## Gates (local)
- `cargo fmt --all --check`: pass.
- `cargo clippy -p opensvn-compat -p xtask --all-targets -D warnings`: pass. Scoped to the touched
  crates; a workspace-wide clippy was not run because the shared host was loaded with other agents'
  builds.
- `cargo test -p opensvn-compat -p xtask`: 29 passed, 0 failed, 1 ignored (Docker).
- The workflow YAML parses.

## Not executed
- **Docker is unavailable on this host**: `docker info` reports no daemon at /var/run/docker.sock.
  So none of the new scenarios have been run against real svn: not the default
  `format32-refused-svn14`, not the extended legacy smokes, and not the bake target list.
  Assumptions to confirm on the first Docker run:
  - 1.8-1.14 report format 32 as `E155021` (SVN_ERR_WC_UNSUPPORTED_FORMAT, "This client is too
    old ...").
  - svn 1.15.0 `checkout --compatible-version 1.15` produces format 32.
  - The a/c `info` comparison agrees after the old-client commit.
