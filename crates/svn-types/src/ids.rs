//! Small identifier and value types.

use std::fmt;

use crate::error::{ErrorCode, Result, SvnError};
use crate::rel_path::RelPath;

/// Repository revision number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Revnum(pub u64);

impl fmt::Display for Revnum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "r{}", self.0)
    }
}

/// Microseconds since the Unix epoch (same unit as `apr_time_t`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(pub i64);

/// Handle of an open directory inside a delta editor drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DirId(pub u32);

/// Handle of an open file inside a delta editor drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FileId(pub u32);

/// Validated repository URL (http or https, no query or fragment, no trailing slash).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RepoUrl(url::Url);

impl RepoUrl {
    /// Parses and normalizes a repository URL.
    pub fn parse(s: &str) -> Result<Self> {
        let mut url = url::Url::parse(s).map_err(|e| {
            SvnError::new(ErrorCode::Internal, format!("invalid repository URL: {e}"))
        })?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(SvnError::new(
                ErrorCode::ServerUnsupported,
                format!("unsupported URL scheme: {}", url.scheme()),
            ));
        }
        if url.host_str().is_none() || url.query().is_some() || url.fragment().is_some() {
            return Err(SvnError::new(
                ErrorCode::Internal,
                "repository URL must have a host and no query or fragment",
            ));
        }
        let trimmed = url.path().trim_end_matches('/').to_owned();
        url.set_path(&trimmed);
        Ok(Self(url))
    }

    /// Underlying URL.
    pub fn as_url(&self) -> &url::Url {
        &self.0
    }

    /// Appends a validated relative path, percent-encoding each segment.
    pub fn join(&self, rel: &RelPath) -> Result<Self> {
        let mut url = self.0.clone();
        {
            let mut segs = url
                .path_segments_mut()
                .map_err(|()| SvnError::new(ErrorCode::Internal, "URL cannot be a base"))?;
            segs.pop_if_empty();
            for c in rel.components() {
                segs.push(c);
            }
        }
        Ok(Self(url))
    }
}

impl fmt::Display for RepoUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.as_str())
    }
}

/// Credential scope: credentials are stored and looked up per origin and realm.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OriginKey {
    /// URL scheme (`https`).
    pub scheme: String,
    /// Lower-case host name.
    pub host: String,
    /// Port (explicit or scheme default).
    pub port: u16,
    /// Authentication realm announced by the server.
    pub realm: String,
}
