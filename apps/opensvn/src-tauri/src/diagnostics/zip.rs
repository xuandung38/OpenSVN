//! Minimal ZIP writer (stored entries, no compression): enough for a small diagnostics bundle
//! and free of an extra dependency. All names must be relative, `/`-separated, without `..`.

/// CRC-32 (IEEE) of `data`.
pub fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (i, slot) in table.iter_mut().enumerate() {
        let mut c = i as u32;
        for _ in 0..8 {
            c = if c & 1 == 1 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
        *slot = c;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for b in data {
        crc = table[((crc ^ u32::from(*b)) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

/// Why an entry cannot be written.
#[derive(Debug, PartialEq, Eq)]
pub enum ZipError {
    /// The entry name is not a safe relative path.
    BadName(String),
    /// An entry or the whole archive exceeds the 4 GiB ZIP32 limit.
    TooLarge,
}

fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.contains('\\')
        && !name.contains('\0')
        && name
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
}

fn le16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}
fn le32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Builds an archive from `(name, bytes)` pairs (fixed 2026-01-01 timestamps keep it reproducible).
pub fn write_zip(entries: &[(String, Vec<u8>)]) -> Result<Vec<u8>, ZipError> {
    // DOS date 2026-01-01, time 00:00.
    const DOS_DATE: u16 = ((2026 - 1980) << 9) | (1 << 5) | 1;
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        if !safe_name(name) {
            return Err(ZipError::BadName(name.clone()));
        }
        let size = u32::try_from(data.len()).map_err(|_| ZipError::TooLarge)?;
        let offset = u32::try_from(out.len()).map_err(|_| ZipError::TooLarge)?;
        let crc = crc32(data);
        let name_len = u16::try_from(name.len()).map_err(|_| ZipError::BadName(name.clone()))?;
        // local file header
        le32(&mut out, 0x0403_4B50);
        le16(&mut out, 20);
        le16(&mut out, 0x0800); // UTF-8 names
        le16(&mut out, 0); // stored
        le16(&mut out, 0);
        le16(&mut out, DOS_DATE);
        le32(&mut out, crc);
        le32(&mut out, size);
        le32(&mut out, size);
        le16(&mut out, name_len);
        le16(&mut out, 0);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        // central directory header
        le32(&mut central, 0x0201_4B50);
        le16(&mut central, 20);
        le16(&mut central, 20);
        le16(&mut central, 0x0800);
        le16(&mut central, 0);
        le16(&mut central, 0);
        le16(&mut central, DOS_DATE);
        le32(&mut central, crc);
        le32(&mut central, size);
        le32(&mut central, size);
        le16(&mut central, name_len);
        le16(&mut central, 0);
        le16(&mut central, 0);
        le16(&mut central, 0);
        le16(&mut central, 0);
        le32(&mut central, 0);
        le32(&mut central, offset);
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = u32::try_from(out.len()).map_err(|_| ZipError::TooLarge)?;
    let central_len = u32::try_from(central.len()).map_err(|_| ZipError::TooLarge)?;
    let count = u16::try_from(entries.len()).map_err(|_| ZipError::TooLarge)?;
    out.extend_from_slice(&central);
    le32(&mut out, 0x0605_4B50);
    le16(&mut out, 0);
    le16(&mut out, 0);
    le16(&mut out, count);
    le16(&mut out, count);
    le32(&mut out, central_len);
    le32(&mut out, central_offset);
    le16(&mut out, 0);
    u32::try_from(out.len()).map_err(|_| ZipError::TooLarge)?;
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
pub mod tests_support {
    use super::*;

    /// Reads the archive back through its central directory and verifies every CRC.
    pub fn read_zip(zip: &[u8]) -> Vec<(String, Vec<u8>)> {
        let u16at = |i: usize| u16::from_le_bytes([zip[i], zip[i + 1]]) as usize;
        let u32at = |i: usize| u32::from_le_bytes([zip[i], zip[i + 1], zip[i + 2], zip[i + 3]]);
        let eocd = zip.len() - 22;
        assert_eq!(u32at(eocd), 0x0605_4B50);
        let count = u16at(eocd + 10);
        let mut pos = u32at(eocd + 16) as usize;
        let mut out = Vec::new();
        for _ in 0..count {
            assert_eq!(u32at(pos), 0x0201_4B50);
            let (crc, size) = (u32at(pos + 16), u32at(pos + 24) as usize);
            let (nlen, xlen, clen) = (u16at(pos + 28), u16at(pos + 30), u16at(pos + 32));
            let local = u32at(pos + 42) as usize;
            let name = String::from_utf8(zip[pos + 46..pos + 46 + nlen].to_vec()).unwrap();
            assert_eq!(u32at(local), 0x0403_4B50);
            let data_start = local + 30 + u16at(local + 26) + u16at(local + 28);
            let data = zip[data_start..data_start + size].to_vec();
            assert_eq!(crc32(&data), crc, "crc of {name}");
            out.push((name, data));
            pos += 46 + nlen + xlen + clen;
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::tests_support::read_zip;
    use super::*;

    #[test]
    fn crc32_matches_the_known_check_value() {
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn archive_round_trips() {
        let entries = vec![
            ("app-info.txt".to_owned(), b"hello".to_vec()),
            (
                "logs/opensvn.log".to_owned(),
                "tiếng Việt\n".as_bytes().to_vec(),
            ),
            ("empty".to_owned(), Vec::new()),
        ];
        let zip = write_zip(&entries).unwrap();
        assert_eq!(read_zip(&zip), entries);
    }

    #[test]
    fn unsafe_names_are_refused() {
        for bad in ["", "/abs", "../x", "a/../b", "a//b", "a\\b", "./a"] {
            assert_eq!(
                write_zip(&[(bad.to_owned(), vec![])]),
                Err(ZipError::BadName(bad.to_owned())),
                "{bad:?}"
            );
        }
    }
}
