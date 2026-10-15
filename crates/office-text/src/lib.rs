//! Plain-text views of Office files, so two versions of a document can be compared with a line
//! diff (like TortoiseSVN's `diff-doc` / `diff-xls` scripts, without Office installed).
//!
//! * Word (`.docx`, `.docm`, `.dotx`, `.dotm`): one line per paragraph of the body, table rows as
//!   `| cell | cell |` lines, then headers, footers, footnotes, endnotes and comments in their
//!   own sections. Images, drawings and formatting are left out.
//! * Excel (`.xlsx`, `.xlsm`, `.xltx`, `.xltm`): one line per non-empty cell, `Sheet!A1: value`,
//!   with the formula after the value when the cell has one. Numbers are shown as stored (a date
//!   is its serial number); number formats are not applied.
//! * The legacy binary formats (`.doc`, `.xls`) are recognized and refused with
//!   [`ExtractError::Legacy`].
//!
//! Only the text parts of the package are read, streamed from the zip entry: a 100 MB document
//! whose size is all embedded pictures costs no more than its text. Hostile packages are bounded
//! by [`Limits`]: number of entries, decompressed bytes per entry and in total (zip bombs), XML
//! nesting depth and size of the produced text. A DTD is refused (no entity expansion).

mod docx;
mod package;
mod rels;
mod xlsx;
mod xml;

use std::io::{Read, Seek};

/// Office file families this crate knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfficeFormat {
    /// Word, Office Open XML.
    Docx,
    /// Excel, Office Open XML.
    Xlsx,
    /// Word 97-2003 binary document (not supported).
    LegacyDoc,
    /// Excel 97-2003 binary workbook (not supported).
    LegacyXls,
}

impl OfficeFormat {
    /// Format implied by the extension of the last segment of `path` (case-insensitive).
    #[must_use]
    pub fn from_path(path: &str) -> Option<Self> {
        let name = path.rsplit(['/', '\\']).next()?;
        let (_, ext) = name.rsplit_once('.')?;
        match ext.to_ascii_lowercase().as_str() {
            "docx" | "docm" | "dotx" | "dotm" => Some(Self::Docx),
            "xlsx" | "xlsm" | "xltx" | "xltm" => Some(Self::Xlsx),
            "doc" | "dot" => Some(Self::LegacyDoc),
            "xls" | "xlt" => Some(Self::LegacyXls),
            _ => None,
        }
    }

    /// Whether the text of this format can be extracted.
    #[must_use]
    pub fn is_supported(self) -> bool {
        matches!(self, Self::Docx | Self::Xlsx)
    }
}

/// Bounds applied while reading a package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Most entries in the zip directory.
    pub max_entries: usize,
    /// Most decompressed bytes read from one entry.
    pub max_entry_bytes: u64,
    /// Most decompressed bytes read from all entries together.
    pub max_total_bytes: u64,
    /// Deepest XML element nesting.
    pub max_depth: usize,
    /// Most bytes of produced text.
    pub max_output_bytes: usize,
    /// Most shared strings of a workbook.
    pub max_shared_strings: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_entries: 20_000,
            max_entry_bytes: 512 * 1024 * 1024,
            max_total_bytes: 1024 * 1024 * 1024,
            max_depth: 256,
            max_output_bytes: 64 * 1024 * 1024,
            max_shared_strings: 2_000_000,
        }
    }
}

/// Why no text could be produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtractError {
    /// A Word 97-2003 or Excel 97-2003 binary file.
    Legacy(OfficeFormat),
    /// Not a zip package: a legacy file with a new extension, a password-protected document
    /// (stored as an encrypted compound file) or a damaged one.
    NotAPackage,
    /// The package lacks a part every file of its kind has.
    MissingPart(String),
    /// A part is stored encrypted.
    Encrypted(String),
    /// A limit of [`Limits`] was reached.
    LimitExceeded(&'static str),
    /// A part is not well-formed XML (or uses a DTD).
    BadXml(String),
    /// Reading the package failed.
    Io(String),
}

impl std::fmt::Display for ExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Legacy(OfficeFormat::LegacyXls) => {
                f.write_str("Excel 97-2003 workbooks (.xls) are not supported")
            }
            Self::Legacy(_) => f.write_str("Word 97-2003 documents (.doc) are not supported"),
            Self::NotAPackage => f.write_str(
                "not an Office Open XML file (it may be password-protected, damaged or in an older format)",
            ),
            Self::MissingPart(p) => write!(f, "the file has no {p} part"),
            Self::Encrypted(p) => write!(f, "the part {p} is encrypted"),
            Self::LimitExceeded(what) => write!(f, "the file is too large to read: {what}"),
            Self::BadXml(e) => write!(f, "the file is damaged: {e}"),
            Self::Io(e) => write!(f, "cannot read the file: {e}"),
        }
    }
}

impl std::error::Error for ExtractError {}

/// Signature of an OLE compound file (`.doc`, `.xls`, and encrypted Office Open XML files).
const OLE_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

/// Extracts the text of an Office file of `format` read from `reader`.
///
/// # Errors
/// See [`ExtractError`]; the legacy formats always fail with [`ExtractError::Legacy`].
pub fn extract_text<R: Read + Seek>(
    mut reader: R,
    format: OfficeFormat,
    limits: &Limits,
) -> Result<String, ExtractError> {
    match format {
        OfficeFormat::LegacyDoc | OfficeFormat::LegacyXls => Err(ExtractError::Legacy(format)),
        OfficeFormat::Docx | OfficeFormat::Xlsx => {
            if starts_with_ole_magic(&mut reader)? {
                return Err(ExtractError::NotAPackage);
            }
            let mut pkg = package::Package::open(reader, limits)?;
            let mut out = xml::Output::new(limits.max_output_bytes);
            if format == OfficeFormat::Docx {
                docx::extract(&mut pkg, &mut out)?;
            } else {
                xlsx::extract(&mut pkg, &mut out)?;
            }
            Ok(out.into_string())
        }
    }
}

fn starts_with_ole_magic<R: Read + Seek>(reader: &mut R) -> Result<bool, ExtractError> {
    let io = |e: std::io::Error| ExtractError::Io(e.to_string());
    let mut head = [0u8; 8];
    let mut got = 0;
    while got < head.len() {
        match reader.read(&mut head[got..]).map_err(io)? {
            0 => break,
            n => got += n,
        }
    }
    reader.seek(std::io::SeekFrom::Start(0)).map_err(io)?;
    Ok(got == head.len() && head == OLE_MAGIC)
}

#[cfg(test)]
mod tests;
