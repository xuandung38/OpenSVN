//! Capability-driven choices. Nothing here looks at a server version: every decision comes from
//! what the server announced in `OPTIONS`, so mod_dav_svn 1.8 through 1.15 are handled by the
//! same code and unknown future capabilities are simply ignored.

use super::options::{BulkUpdates, Capabilities};

/// How to list a directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListStrategy {
    /// One `list-report` request (capability `list`).
    ListReport,
    /// `Depth: 1` PROPFIND (works on every server).
    PropfindDepth1,
}

/// What the client would like for updates, before the server's wishes are applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkPreference {
    /// Follow the server: bulk when it says `Prefer`, skelta otherwise.
    Auto,
    /// Ask for bulk (send-all) updates when the server permits them.
    Bulk,
    /// Never use bulk updates.
    Skelta,
}

impl Capabilities {
    /// Whether the server lets clients use send-all updates. A server that does not send
    /// `SVN-Allow-Bulk-Updates` (the switch was added later) imposes no restriction.
    pub fn bulk_updates_allowed(&self) -> bool {
        self.bulk_updates != Some(BulkUpdates::Off)
    }

    /// svndiff version for `PUT`, with serf's preference: version 1 (zlib) whenever the server
    /// has it, because it compresses better over a WAN; version 2 (LZ4) only on a low-latency
    /// connection, where speed matters more; version 0 when the server announced neither.
    pub fn svndiff_version(&self, low_latency: bool) -> u8 {
        match (self.dav.svndiff1, self.dav.svndiff2) {
            (_, true) if low_latency || !self.dav.svndiff1 => 2,
            (true, _) => 1,
            _ => 0,
        }
    }

    /// `list-report` where announced, otherwise a Depth-1 PROPFIND.
    pub fn list_strategy(&self) -> ListStrategy {
        if self.dav.list {
            ListStrategy::ListReport
        } else {
            ListStrategy::PropfindDepth1
        }
    }

    /// Whether `PUT` may carry `X-SVN-Result-Fulltext-MD5` for the server to verify.
    pub fn verifies_put_result_checksum(&self) -> bool {
        self.dav.put_result_checksum
    }
}
