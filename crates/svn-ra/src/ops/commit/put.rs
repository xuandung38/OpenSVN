//! Upload of one file's text: svndiff full-text spill file, then a replayable `PUT`.
//!
//! The svndiff stream is written to a temporary file first. That keeps memory flat for any file
//! size, gives the exact `Content-Length`, lets the transport send the body again after a 401
//! or a connection reset, and lets us check the content against the checksum the caller
//! promised before a single byte goes to the server.

use std::io::{BufWriter, Read, Write};
use std::sync::Arc;

use md5::{Digest, Md5};
use svn_delta::{SvndiffEncoder, SvndiffVersion, WINDOW_SIZE};
use svn_protocol::commit::{LockToken, PutOptions, put_request};
use svn_protocol::{Body, TxnName};
use svn_ra_api::ReplayableSource;
use svn_types::{CancelToken, Checksum, ErrorCode, RelPath, Result, Revnum, SvnError};
use tempfile::NamedTempFile;
use tokio::sync::Semaphore;

use crate::ops::update::RaWire;
use crate::transport::RequestBody;

/// Lower-case hex of an MD5 digest.
pub(super) fn md5_hex(digest: &[u8; 16]) -> String {
    use std::fmt::Write as _;
    digest.iter().fold(String::with_capacity(32), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// An encoded svndiff stream waiting on disk.
struct Spill {
    file: NamedTempFile,
    size: u64,
}

fn io_err(what: &str, e: &std::io::Error) -> SvnError {
    SvnError::from_io(what, e)
}

/// Reads up to `buf.len()` bytes, stopping early only at the end of the file.
fn fill(src: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match src.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// Encodes `source` as a full-text svndiff stream, checking size and MD5 on the way.
fn spill(source: &ReplayableSource, version: SvndiffVersion, expected: &[u8; 16]) -> Result<Spill> {
    let mut input = source
        .open()
        .map_err(|e| io_err("cannot open file to commit", &e))?;
    let tmp = NamedTempFile::new().map_err(|e| io_err("cannot create upload spill file", &e))?;
    let mut out = BufWriter::new(tmp);
    let mut encoder = SvndiffEncoder::new(version);
    let mut hasher = Md5::new();
    let mut buf = vec![0_u8; WINDOW_SIZE];
    let mut encoded = Vec::new();
    let mut total = 0_u64;
    let mut size = 0_u64;
    // The header is written even for an empty file.
    encoder.write_header(&mut encoded);
    loop {
        let n = fill(&mut input, &mut buf).map_err(|e| io_err("cannot read file to commit", &e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
        encoder.write_fulltext(&buf[..n], &mut encoded)?;
        out.write_all(&encoded)
            .map_err(|e| io_err("cannot write upload spill file", &e))?;
        size += encoded.len() as u64;
        encoded.clear();
    }
    out.write_all(&encoded)
        .map_err(|e| io_err("cannot write upload spill file", &e))?;
    size += encoded.len() as u64;
    let tmp = out
        .into_inner()
        .map_err(|e| io_err("cannot write upload spill file", e.error()))?;
    if let Some(expected_len) = source.size
        && expected_len != total
    {
        return Err(SvnError::new(
            ErrorCode::Corrupt,
            format!("file changed while committing: expected {expected_len} bytes, read {total}"),
        ));
    }
    let digest: [u8; 16] = hasher.finalize().into();
    if &digest != expected {
        return Err(SvnError::new(
            ErrorCode::Corrupt,
            "file changed while committing: its content does not match the expected checksum",
        ));
    }
    Ok(Spill { file: tmp, size })
}

fn version_of(wire: &RaWire) -> SvndiffVersion {
    match wire.caps().svndiff_version(false) {
        2 => SvndiffVersion::V2,
        1 => SvndiffVersion::V1,
        _ => SvndiffVersion::V0,
    }
}

/// Everything an upload task needs; owned so the task can outlive the editor call.
pub(super) struct PutJob {
    pub wire: RaWire,
    pub txn: TxnName,
    /// Repository-relative path of the file.
    pub path: RelPath,
    pub target: ReplayableSource,
    pub base_rev: Option<Revnum>,
    pub base_md5: Option<[u8; 16]>,
    pub result_md5: [u8; 16],
    pub lock: Option<String>,
    pub slots: Arc<Semaphore>,
    pub cancel: CancelToken,
}

/// Checks that a checksum can be verified here (MD5 only).
pub(super) fn require_md5(c: &Checksum, what: &str) -> Result<[u8; 16]> {
    match c {
        Checksum::Md5(d) => Ok(*d),
        Checksum::Sha1(_) => Err(SvnError::new(
            ErrorCode::Internal,
            format!("{what} must be an MD5 checksum"),
        )),
    }
}

/// Runs one upload: encode to a spill file, `PUT` it, check the answer.
pub(super) async fn run_put(job: PutJob) -> Result<()> {
    let _slot = tokio::select! {
        biased;
        () = job.cancel.cancelled() => return job.cancel.check(),
        p = job.slots.clone().acquire_owned() => p.map_err(|_| {
            SvnError::new(ErrorCode::Internal, "upload scheduler closed")
        })?,
    };
    job.cancel.check()?;
    let version = version_of(&job.wire);
    let (target, expected) = (job.target.clone(), job.result_md5);
    let spill = tokio::task::spawn_blocking(move || spill(&target, version, &expected))
        .await
        .map_err(|e| SvnError::new(ErrorCode::Internal, format!("encode task failed: {e}")))??;
    send_put(&job, &spill).await
}

async fn send_put(job: &PutJob, spill: &Spill) -> Result<()> {
    let base_hex = job.base_md5.as_ref().map(md5_hex);
    let result_hex = md5_hex(&job.result_md5);
    let opts = PutOptions {
        base_rev: job.base_rev,
        base_md5: base_hex.as_deref(),
        result_md5: None,
        lock: job.lock.as_deref().map(LockToken),
    }
    .with_result_md5(job.wire.caps(), &result_hex);
    let req = put_request(job.wire.urls(), &job.txn, &job.path, Body::Empty, &opts)?;
    let source = ReplayableSource {
        path: spill.file.path().to_path_buf(),
        translated: true,
        size: Some(spill.size),
    };
    let resp = job
        .wire
        .send_with_body(req, RequestBody::File(source), &job.cancel)
        .await?;
    RaWire::require_success(resp, &job.cancel).await?;
    Ok(())
}

/// Creates an empty file (`PUT` with an empty body).
pub(super) async fn put_empty(
    wire: &RaWire,
    txn: &TxnName,
    path: &RelPath,
    lock: Option<&str>,
    cancel: &CancelToken,
) -> Result<()> {
    let opts = PutOptions {
        lock: lock.map(LockToken),
        ..PutOptions::default()
    };
    let req = put_request(wire.urls(), txn, path, Body::Empty, &opts)?;
    wire.send_ok(req, cancel).await?;
    Ok(())
}
