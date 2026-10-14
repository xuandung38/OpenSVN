//! Upload of one file's text: an svndiff stream (a delta against the BASE text when there is
//! one, otherwise the full text), then a replayable `PUT`.
//!
//! The stream is encoded completely before anything is sent: small streams stay in memory,
//! larger ones go to a temporary file. That keeps memory flat for any file size, gives the exact
//! `Content-Length`, lets the transport send the body again after a 401 or a connection reset,
//! and lets us check the content against the checksums the caller promised before a single byte
//! goes to the server.
//!
//! A delta is only sent when the base text read here has exactly the size and MD5 the caller
//! named: the server applies the delta to its own copy of the base (after checking the
//! `X-SVN-Base-Fulltext-MD5` header against it), so a damaged or stale local base would
//! otherwise produce a wrong file. When the local base does not match, the full text is sent
//! instead. The server additionally checks the result against `X-SVN-Result-Fulltext-MD5` where
//! it announces that.

use std::io::{BufWriter, Read, Write};
use std::sync::Arc;

use bytes::Bytes;
use md5::{Digest, Md5};
use svn_delta::{SvndiffEncoder, SvndiffVersion, WINDOW_SIZE, delta_window};
use svn_protocol::commit::{LockToken, PutOptions, put_request};
use svn_protocol::{Body, TxnName};
use svn_ra_api::ReplayableSource;
use svn_types::{CancelToken, Checksum, ErrorCode, RelPath, Result, Revnum, SvnError};
use tempfile::NamedTempFile;
use tokio::sync::Semaphore;

use crate::ops::update::RaWire;
use crate::transport::RequestBody;

/// Encoded streams up to this size are kept in memory instead of a temporary file.
const MEMORY_LIMIT: usize = 1 << 20;

/// Lower-case hex of an MD5 digest.
pub(super) fn md5_hex(digest: &[u8; 16]) -> String {
    use std::fmt::Write as _;
    digest.iter().fold(String::with_capacity(32), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// An encoded svndiff stream waiting to be sent.
pub(super) enum Spill {
    Memory(Bytes),
    File { file: NamedTempFile, size: u64 },
}

impl Spill {
    fn body(&self) -> RequestBody {
        match self {
            Self::Memory(b) => RequestBody::Bytes(b.clone()),
            Self::File { file, size } => RequestBody::File(ReplayableSource {
                path: file.path().to_path_buf(),
                translated: true,
                size: Some(*size),
            }),
        }
    }
}

/// Collects the encoded stream in memory and moves it to a temporary file once it grows past
/// [`MEMORY_LIMIT`].
struct SpillWriter {
    mem: Vec<u8>,
    file: Option<BufWriter<NamedTempFile>>,
    size: u64,
}

impl SpillWriter {
    fn new() -> Self {
        Self {
            mem: Vec::new(),
            file: None,
            size: 0,
        }
    }

    fn write(&mut self, data: &[u8]) -> Result<()> {
        self.size += data.len() as u64;
        if self.file.is_none() && self.mem.len() + data.len() <= MEMORY_LIMIT {
            self.mem.extend_from_slice(data);
            return Ok(());
        }
        let out = match &mut self.file {
            Some(f) => f,
            None => {
                let tmp = NamedTempFile::new()
                    .map_err(|e| io_err("cannot create upload spill file", &e))?;
                let mut f = BufWriter::new(tmp);
                f.write_all(&self.mem)
                    .map_err(|e| io_err("cannot write upload spill file", &e))?;
                self.mem = Vec::new();
                self.file.insert(f)
            }
        };
        out.write_all(data)
            .map_err(|e| io_err("cannot write upload spill file", &e))
    }

    fn finish(self) -> Result<Spill> {
        match self.file {
            None => Ok(Spill::Memory(Bytes::from(self.mem))),
            Some(f) => {
                let file = f
                    .into_inner()
                    .map_err(|e| io_err("cannot write upload spill file", e.error()))?;
                Ok(Spill::File {
                    file,
                    size: self.size,
                })
            }
        }
    }
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

/// The base text a delta is computed against, with what the caller says it is.
pub(super) struct DeltaBase<'a> {
    pub source: &'a ReplayableSource,
    pub md5: [u8; 16],
}

/// Reads the base text window by window, hashing everything it reads.
struct BaseReader {
    file: std::fs::File,
    hasher: Md5,
    total: u64,
    offset: u64,
    buf: Vec<u8>,
}

impl BaseReader {
    /// The next source view (empty once the base is exhausted).
    fn next_view(&mut self) -> Result<(u64, &[u8])> {
        let n = fill(&mut self.file, &mut self.buf)
            .map_err(|e| io_err("cannot read the base text", &e))?;
        self.hasher.update(&self.buf[..n]);
        self.total += n as u64;
        let offset = self.offset;
        self.offset += n as u64;
        Ok((offset, &self.buf[..n]))
    }

    /// Reads the rest and says whether the whole base has the expected size and MD5.
    fn matches(mut self, base: &DeltaBase<'_>) -> Result<bool> {
        loop {
            let n = fill(&mut self.file, &mut self.buf)
                .map_err(|e| io_err("cannot read the base text", &e))?;
            if n == 0 {
                break;
            }
            self.hasher.update(&self.buf[..n]);
            self.total += n as u64;
        }
        let digest: [u8; 16] = self.hasher.finalize().into();
        Ok(digest == base.md5 && base.source.size.is_none_or(|s| s == self.total))
    }
}

/// Encodes `target` as an svndiff stream, against `base` when given, checking the target's
/// size and MD5 on the way. Returns the stream and whether it is a delta against the base;
/// `false` when there was no base or the base on disk is not the one the caller named (then
/// the stream is the full text).
pub(super) fn encode(
    target: &ReplayableSource,
    base: Option<&DeltaBase<'_>>,
    version: SvndiffVersion,
    expected: &[u8; 16],
) -> Result<(Spill, bool)> {
    let mut base_reader = match base {
        Some(b) => match b.source.open() {
            Ok(file) => Some(BaseReader {
                file,
                hasher: Md5::new(),
                total: 0,
                offset: 0,
                buf: vec![0_u8; WINDOW_SIZE],
            }),
            // A missing pristine only costs bandwidth.
            Err(_) => None,
        },
        None => None,
    };
    let mut input = target
        .open()
        .map_err(|e| io_err("cannot open file to commit", &e))?;
    let mut out = SpillWriter::new();
    let mut encoder = SvndiffEncoder::new(version);
    let mut hasher = Md5::new();
    let mut buf = vec![0_u8; WINDOW_SIZE];
    let mut encoded = Vec::new();
    let mut total = 0_u64;
    // The header is written even for an empty file.
    encoder.write_header(&mut encoded);
    loop {
        let n = fill(&mut input, &mut buf).map_err(|e| io_err("cannot read file to commit", &e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
        match &mut base_reader {
            Some(r) => {
                let (offset, view) = r.next_view()?;
                encoder.write_window(&delta_window(offset, view, &buf[..n]), &mut encoded)?;
            }
            None => encoder.write_fulltext(&buf[..n], &mut encoded)?,
        }
        out.write(&encoded)?;
        encoded.clear();
    }
    out.write(&encoded)?;
    if let Some(expected_len) = target.size
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
    match (base_reader, base) {
        (Some(r), Some(b)) => {
            if r.matches(b)? {
                Ok((out.finish()?, true))
            } else {
                // The local base is not the text the server will apply the delta to.
                drop(out);
                Ok((encode(target, None, version, expected)?.0, false))
            }
        }
        _ => Ok((out.finish()?, false)),
    }
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
    /// The BASE text to compute the delta against (`None`: send the full text).
    pub base: Option<ReplayableSource>,
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

/// Runs one upload: encode (a delta when the base can be used), `PUT` it, check the answer.
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
    // Without the base checksum the server cannot check the base, so no delta.
    let base = job.base.clone().zip(job.base_md5);
    let (spill, _delta) = tokio::task::spawn_blocking(move || {
        let base = base
            .as_ref()
            .map(|(source, md5)| DeltaBase { source, md5: *md5 });
        encode(&target, base.as_ref(), version, &expected)
    })
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
    let resp = job
        .wire
        .send_with_body(req, spill.body(), &job.cancel)
        .await?;
    RaWire::require_success(resp, &job.cancel).await?;
    Ok(())
}

/// Creates an empty file: the server expects a svndiff stream, here just its header.
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
    let header = SvndiffEncoder::fulltext(version_of(wire), &[])?;
    let req = put_request(wire.urls(), txn, path, Body::Bytes(header.into()), &opts)?;
    wire.send_ok(req, cancel).await?;
    Ok(())
}
