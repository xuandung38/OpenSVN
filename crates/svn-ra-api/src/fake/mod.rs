//! In-memory test doubles (feature `fake`): a repository that implements the whole
//! [`RaSession`](crate::RaSession) contract, an editor that verifies ordering rules and a no-op reporter.

mod commit_editor;
mod recording_editor;
mod repo;
mod session;
mod update_drive;
mod update_plan;

pub use recording_editor::{EditorEvent, RecordingEditor, RecordingLog};
pub use repo::{FakeChange, FakeMergeFault, Node, NodeData, Revision, Tree};
pub use session::FakeRaSession;

use svn_types::{Depth, RelPath, RepoUrl, Reporter, Result, Revnum};

/// Reporter that accepts everything and reports nothing.
#[derive(Debug, Default, Clone, Copy)]
pub struct NullReporter;

impl Reporter for NullReporter {
    fn set_path(
        &mut self,
        _: &RelPath,
        _: Revnum,
        _: Depth,
        _: bool,
        _: Option<&str>,
    ) -> Result<()> {
        Ok(())
    }
    fn delete_path(&mut self, _: &RelPath) -> Result<()> {
        Ok(())
    }
    fn link_path(
        &mut self,
        _: &RelPath,
        _: &RepoUrl,
        _: Revnum,
        _: Depth,
        _: bool,
        _: Option<&str>,
    ) -> Result<()> {
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        Ok(())
    }
    fn abort(&mut self) -> Result<()> {
        Ok(())
    }
}
