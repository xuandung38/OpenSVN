//! Keyword table construction (`svn_subst_build_keywords3` and `keyword_printf`).

use std::collections::BTreeMap;

/// Values available to keyword expansion for one file. Mirrors the arguments svn passes to
/// `svn_subst_build_keywords3`; absent values expand to nothing, like NULL in C.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeywordContext {
    /// Last changed revision rendered by `%r` (svn passes `-1` when unknown).
    pub rev: Option<i64>,
    /// Full, URI-encoded URL of the file (`%u`).
    pub url: Option<String>,
    /// Repository root URL, URI-encoded (`%R`).
    pub repos_root_url: Option<String>,
    /// Last changed date in microseconds since the Unix epoch; `None` or `0` means unknown.
    pub date_micros: Option<i64>,
    /// Last changed author (`%a`).
    pub author: Option<String>,
    /// Local UTC offset in seconds in effect at `date_micros`. svn formats `$Date$`
    /// (`%D`) in the local time zone; the caller resolves the zone because this crate has
    /// no time-zone database. Short dates (`%d`, used by `$Id$`/`$Header$`) are always UTC.
    pub utc_offset_secs: i32,
}

/// Keyword name to expansion value, exactly as svn's `apr_hash_t` of keywords.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Keywords {
    map: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl Keywords {
    /// Value for `name` (case-sensitive), if enabled.
    pub fn get(&self, name: &[u8]) -> Option<&[u8]> {
        self.map.get(name).map(Vec::as_slice)
    }

    /// `true` when no keyword is enabled.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    fn set(&mut self, name: &str, value: &[u8]) {
        self.map.insert(name.as_bytes().to_vec(), value.to_vec());
    }

    /// Builds the table from an `svn:keywords` value (`svn_subst_build_keywords3`, custom
    /// `name=format` entries enabled).
    pub fn build(keywords_prop: &[u8], ctx: &KeywordContext) -> Self {
        let mut kw = Self::default();
        const SEPS: &[u8] = b" \t\x0b\n\x08\r\x0c";
        for token in keywords_prop
            .split(|b| SEPS.contains(b))
            .filter(|t| !t.is_empty())
        {
            if let Some(eq) = token.iter().position(|b| *b == b'=') {
                let value = keyword_printf(&token[eq + 1..], ctx);
                kw.map.insert(token[..eq].to_vec(), value);
                continue;
            }
            let ci = |s: &str| token.eq_ignore_ascii_case(s.as_bytes());
            let ex = |s: &str| token == s.as_bytes();
            if ex("LastChangedRevision") || ex("Revision") || ci("Rev") {
                let v = keyword_printf(b"%r", ctx);
                for n in ["LastChangedRevision", "Revision", "Rev"] {
                    kw.set(n, &v);
                }
            } else if ex("LastChangedDate") || ci("Date") {
                let v = keyword_printf(b"%D", ctx);
                for n in ["LastChangedDate", "Date"] {
                    kw.set(n, &v);
                }
            } else if ex("LastChangedBy") || ci("Author") {
                let v = keyword_printf(b"%a", ctx);
                for n in ["LastChangedBy", "Author"] {
                    kw.set(n, &v);
                }
            } else if ex("HeadURL") || ci("URL") {
                let v = keyword_printf(b"%u", ctx);
                for n in ["HeadURL", "URL"] {
                    kw.set(n, &v);
                }
            } else if ci("Id") {
                kw.set("Id", &keyword_printf(b"%b %r %d %a", ctx));
            } else if ci("Header") {
                kw.set("Header", &keyword_printf(b"%u %r %d %a", ctx));
            }
        }
        kw
    }

    /// Table used to contract keywords when normalising: only the key set matters, the
    /// values are never inserted (svn builds it with empty URL, revision `-1`, no date).
    pub fn for_normalization(keywords_prop: &[u8]) -> Self {
        let ctx = KeywordContext {
            rev: Some(-1),
            url: Some(String::new()),
            repos_root_url: Some(String::new()),
            date_micros: None,
            author: Some(String::new()),
            utc_offset_secs: 0,
        };
        Self::build(keywords_prop, &ctx)
    }
}

/// Implementation of svn's `keyword_printf`.
pub(crate) fn keyword_printf(fmt: &[u8], ctx: &KeywordContext) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < fmt.len() {
        if fmt[i] != b'%' {
            out.push(fmt[i]);
            i += 1;
            continue;
        }
        let Some(&code) = fmt.get(i + 1) else {
            out.push(b'%');
            break;
        };
        i += 2;
        match code {
            b'a' => out.extend(ctx.author.as_deref().unwrap_or("").as_bytes()),
            b'b' => {
                if let Some(url) = ctx.url.as_deref().filter(|u| !u.is_empty()) {
                    out.extend(uri_basename(url));
                }
            }
            b'd' => {
                if let Some(d) = date(ctx) {
                    let t = Civil::from_micros(d, 0);
                    out.extend(
                        format!(
                            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z",
                            t.year, t.month, t.day, t.hour, t.min, t.sec
                        )
                        .into_bytes(),
                    );
                }
            }
            b'D' => {
                if let Some(d) = date(ctx) {
                    out.extend(human_date(d, ctx.utc_offset_secs).into_bytes());
                }
            }
            b'P' => {
                if let (Some(root), Some(url)) = (
                    ctx.repos_root_url.as_deref().filter(|r| !r.is_empty()),
                    ctx.url.as_deref().filter(|u| !u.is_empty()),
                ) && let Some(rel) = uri_skip_ancestor(root, url)
                {
                    out.extend(percent_decode(rel.as_bytes()));
                }
            }
            b'R' => out.extend(ctx.repos_root_url.as_deref().unwrap_or("").as_bytes()),
            b'r' => {
                if let Some(r) = ctx.rev {
                    out.extend(r.to_string().into_bytes());
                }
            }
            b'u' => out.extend(ctx.url.as_deref().unwrap_or("").as_bytes()),
            b'_' => out.push(b' '),
            b'%' => out.push(b'%'),
            b'H' => out.extend(keyword_printf(b"%P%_%r%_%d%_%a", ctx)),
            b'I' => out.extend(keyword_printf(b"%b%_%r%_%d%_%a", ctx)),
            other => {
                out.push(b'%');
                out.push(other);
            }
        }
    }
    out
}

/// svn treats a zero `apr_time_t` as "no date".
fn date(ctx: &KeywordContext) -> Option<i64> {
    ctx.date_micros.filter(|d| *d != 0)
}

/// `svn_uri_skip_ancestor` before decoding.
fn uri_skip_ancestor<'a>(parent: &str, child: &'a str) -> Option<&'a str> {
    let rest = child.strip_prefix(parent)?;
    if rest.is_empty() {
        Some("")
    } else {
        rest.strip_prefix('/')
    }
}

/// `svn_uri_basename`: last path segment, percent-decoded (empty for a root URL).
fn uri_basename(url: &str) -> Vec<u8> {
    // A root URI is "scheme://host[:port]" with nothing after the authority.
    let after_scheme = url.split_once("://").map_or(url, |(_, r)| r);
    if !after_scheme.contains('/') {
        return Vec::new();
    }
    let seg = url.rsplit('/').next().unwrap_or("");
    percent_decode(seg.as_bytes())
}

/// `svn_path_uri_decode`: decodes valid `%XX` sequences, leaves everything else.
fn percent_decode(s: &[u8]) -> Vec<u8> {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%'
            && let (Some(h), Some(l)) = (
                s.get(i + 1).and_then(|b| hex(*b)),
                s.get(i + 2).and_then(|b| hex(*b)),
            )
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(s[i]);
        i += 1;
    }
    out
}

/// Broken-down calendar time.
struct Civil {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    min: u32,
    sec: u32,
    /// 0 = Sunday.
    weekday: usize,
}

impl Civil {
    /// Converts microseconds since the epoch plus a UTC offset into calendar fields.
    fn from_micros(micros: i64, offset_secs: i32) -> Self {
        let secs = micros.div_euclid(1_000_000) + i64::from(offset_secs);
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        // Howard Hinnant's civil_from_days.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
        let year = yoe + era * 400 + i64::from(month <= 2);
        Self {
            year,
            month,
            day,
            hour: (rem / 3600) as u32,
            min: (rem % 3600 / 60) as u32,
            sec: (rem % 60) as u32,
            weekday: (days + 4).rem_euclid(7) as usize,
        }
    }
}

/// `svn_time_to_human_cstring` in the C locale: `2002-06-23 11:13:02 +0300 (Sun, 23 Jun 2002)`.
pub(crate) fn human_date(micros: i64, offset_secs: i32) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let t = Civil::from_micros(micros, offset_secs);
    // svn prints gmtoff/3600 with a forced sign (truncating, so -00:30 shows "+00").
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} {:+03}{:02} ({}, {:02} {} {:04})",
        t.year,
        t.month,
        t.day,
        t.hour,
        t.min,
        t.sec,
        offset_secs / 3600,
        (offset_secs.unsigned_abs() / 60) % 60,
        DAYS[t.weekday],
        t.day,
        MONTHS[(t.month - 1) as usize],
        t.year
    )
}
