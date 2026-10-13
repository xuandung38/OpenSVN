//! Cancellation and work budget shared by the LCS and diff3 loops.

use std::fmt;

use svn_types::CancelToken;

/// Why a diff or merge did not complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffError {
    /// The caller's [`CancelToken`] fired.
    Cancelled,
    /// The work budget (`DiffOptions::max_cost`) was exhausted; callers should fall back to
    /// "report a conflict / show an opaque diff" instead of retrying.
    TooComplex,
}

impl fmt::Display for DiffError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("diff cancelled"),
            Self::TooComplex => f.write_str("inputs too different to diff within the cost budget"),
        }
    }
}

impl std::error::Error for DiffError {}

/// Per-call control block: cancellation flag plus a running cost counter.
pub(crate) struct Ctl<'a> {
    cancel: &'a CancelToken,
    max_cost: Option<u64>,
    spent: u64,
}

impl<'a> Ctl<'a> {
    pub(crate) fn new(cancel: &'a CancelToken, max_cost: Option<u64>) -> Self {
        Self {
            cancel,
            max_cost,
            spent: 0,
        }
    }

    /// Accounts `units` of work and reports cancellation or budget exhaustion.
    pub(crate) fn tick(&mut self, units: u64) -> Result<(), DiffError> {
        if self.cancel.is_cancelled() {
            return Err(DiffError::Cancelled);
        }
        self.spent = self.spent.saturating_add(units);
        match self.max_cost {
            Some(max) if self.spent > max => Err(DiffError::TooComplex),
            _ => Ok(()),
        }
    }
}
