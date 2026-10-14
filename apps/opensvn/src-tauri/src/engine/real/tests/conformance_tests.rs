//! The whole shared engine suite (`engine::conformance`) against the real engine, served by the
//! in-memory repository through [`AnchoredConnector`] (sessions anchored at the requested URL, as
//! the HTTP connector hands them out).
//!
//! One test per scenario so a failure names the scenario. Every scenario of
//! `conformance::run_all` passes except the known gaps at the end of this file, which are
//! `should_panic` tests pinned to the exact refusal (reason and tracking issue next to each): when
//! a gap is closed the test fails and must move to the passing list.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use svn_types::SvnError;

use super::support::{
    AnchoredConnector, Fixture, URL, fixture_on, plant_dead_app_lock, plant_lock_row, standard_repo,
};
use crate::engine::EngineFacade;
use crate::engine::conformance::{self, Harness, PlantedOwner};
use crate::engine::real::ClientRecovery;

/// The real engine over the in-memory repository; working copy files are written directly.
struct RealFakeHarness {
    f: Fixture,
    tmp: tempfile::TempDir,
}

fn harness() -> RealFakeHarness {
    let repo = standard_repo();
    let connector = Arc::new(AnchoredConnector::new(repo.clone()));
    RealFakeHarness {
        f: fixture_on(repo, connector, Arc::new(ClientRecovery)),
        tmp: tempfile::tempdir().unwrap(),
    }
}

impl Harness for RealFakeHarness {
    fn engine(&self) -> &dyn EngineFacade {
        &self.f.engine
    }
    fn repo_url(&self) -> String {
        URL.to_owned()
    }
    fn work_dir(&self) -> PathBuf {
        self.tmp.path().join("work")
    }
    fn write_file(&self, root: &Path, rel: &str, content: &str) -> Result<(), SvnError> {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
        Ok(())
    }
    fn remove_file(&self, root: &Path, rel: &str) -> Result<(), SvnError> {
        std::fs::remove_file(root.join(rel)).map_err(|e| SvnError::from_io("remove", &e))
    }
    fn scratch_dir(&self) -> PathBuf {
        self.tmp.path().to_path_buf()
    }
    fn plant_lock(&self, root: &Path, dir: &str, owner: PlantedOwner) -> Result<(), SvnError> {
        match owner {
            PlantedOwner::DeadProcess => plant_dead_app_lock(root, &self.f.store, dir),
            PlantedOwner::Foreign => plant_lock_row(root, dir),
        }
        Ok(())
    }
}

macro_rules! scenario {
    ($name:ident) => {
        #[tokio::test(flavor = "multi_thread")]
        async fn $name() {
            conformance::$name(&harness()).await;
        }
    };
}

scenario!(repository_reads);
scenario!(commit_and_update_cycle);
scenario!(schedule_and_revert);
scenario!(conflict_blocks_commit_until_resolved);
scenario!(lock_and_unlock);
scenario!(remote_edit_and_copy);
scenario!(cancellation_and_errors);
scenario!(working_copy_metadata_operations);
scenario!(switch_to_a_branch);
scenario!(repository_content_tools);
scenario!(open_recovers_dead_locks_and_reports_foreign_ones);
scenario!(locked_unmodified_file_in_status);
scenario!(commit_with_auto_add_remove);
scenario!(update_ends_in_conflict);
scenario!(update_of_sub_paths);

// ---- known gaps ----

/// `mark_move` has no `svn-client` API yet and is refused with a user-facing error
/// (https://github.com/xuandung38/OpenSVN/issues/90); `set_depth` after it is refused the same
/// way (https://github.com/xuandung38/OpenSVN/issues/109).
#[tokio::test(flavor = "multi_thread")]
#[should_panic(expected = "mark_move: SvnError { code: WcInvalidOperation")]
async fn mark_move_and_set_depth_is_a_known_gap() {
    conformance::mark_move_and_set_depth(&harness()).await;
}
