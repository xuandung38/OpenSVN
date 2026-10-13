//! Byte-exact port of the libsvn_subr `translate_chunk` state machine (subst.c, tags
//! 1.14.5 and 1.15.0 are identical here), operating on a whole in-memory buffer.
//!
//! One call is equivalent to feeding svn a single chunk followed by the final flush.

use super::error::TranslateError;
use super::keywords::Keywords;

/// Longest keyword string (`$...$`) that svn recognises (`SVN_KEYWORD_MAX_LEN`).
const KEYWORD_MAX_LEN: usize = 255;

/// Parameters of one translation run.
pub(crate) struct Params<'a> {
    /// End-of-line sequence to write; `None` leaves line endings untouched.
    pub eol: Option<&'a [u8]>,
    /// Convert inconsistent line endings instead of failing.
    pub repair: bool,
    /// Keywords to expand or contract; `None`/empty disables keyword handling.
    pub keywords: Option<&'a Keywords>,
    /// `true` expands keywords, `false` contracts them.
    pub expand: bool,
}

/// Extracts the keyword name of `buf` (`$name$` or `$name: ...$`) and returns it when
/// `keywords` knows it (`match_keyword`).
fn match_keyword<'k>(buf: &'k [u8], keywords: &Keywords) -> Option<&'k [u8]> {
    let len = buf.len();
    let mut i = 0;
    while i + 2 < len && buf[i + 1] != b':' {
        i += 1;
    }
    let mut name = &buf[1..1 + i];
    // C strings end at the first NUL.
    if let Some(nul) = name.iter().position(|b| *b == 0) {
        name = &name[..nul];
    }
    keywords.get(name).map(|_| name)
}

/// Rewrites the value part of an already-recognised keyword after `$name`.
fn write_expanded(buf: &mut Vec<u8>, name_len: usize, value: &[u8]) {
    buf.truncate(1 + name_len);
    buf.extend_from_slice(b": ");
    if value.is_empty() {
        buf.push(b'$');
    } else {
        let max = KEYWORD_MAX_LEN - 5 - name_len;
        buf.extend_from_slice(&value[..value.len().min(max)]);
        buf.extend_from_slice(b" $");
    }
}

/// `translate_keyword_subst`: expands (`value` is `Some`) or contracts (`None`) `buf`.
/// Returns `false` when `buf` is not a well-formed instance of the keyword.
fn subst_keyword(buf: &mut Vec<u8>, keyword: &[u8], value: Option<&[u8]>) -> bool {
    let len = buf.len();
    let kl = keyword.len();
    if len < kl + 2 || kl > KEYWORD_MAX_LEN - 5 || &buf[1..1 + kl] != keyword {
        return false;
    }
    let bp = 1 + kl;
    let at = |b: &[u8], i: usize| b.get(i).copied().unwrap_or(0);

    // Fixed-length form: "$kw:: value   $".
    if at(buf, bp) == b':'
        && at(buf, bp + 1) == b':'
        && at(buf, bp + 2) == b' '
        && (buf[len - 2] == b' ' || buf[len - 2] == b'#')
        && 6 + kl < len
    {
        let max_value = len - (6 + kl);
        match value {
            None => buf[bp + 2..len - 1].fill(b' '),
            Some(v) if v.len() <= max_value => {
                buf[bp + 3..bp + 3 + v.len()].copy_from_slice(v);
                buf[bp + 3 + v.len()..len - 1].fill(b' ');
            }
            Some(v) => {
                buf[bp + 3..bp + 3 + max_value].copy_from_slice(&v[..max_value]);
                buf[len - 2] = b'#';
            }
        }
        return true;
    }
    // Unexpanded: "$kw$".
    if at(buf, bp) == b'$' {
        if let Some(v) = value {
            write_expanded(buf, kl, v);
        }
        return true;
    }
    // Expanded: "$kw: value $" or "$kw:$".
    let expanded =
        (len >= 4 + kl && at(buf, bp) == b':' && at(buf, bp + 1) == b' ' && buf[len - 2] == b' ')
            || (len >= 3 + kl && at(buf, bp) == b':' && at(buf, bp + 1) == b'$');
    if expanded {
        match value {
            None => {
                buf.truncate(2 + kl);
                buf[bp] = b'$';
            }
            Some(v) => write_expanded(buf, kl, v),
        }
        return true;
    }
    false
}

/// Translates `input` according to `params`.
pub(crate) fn translate(input: &[u8], params: &Params<'_>) -> Result<Vec<u8>, TranslateError> {
    let keywords = params.keywords.filter(|k| !k.is_empty());
    let eol = params.eol;
    let interesting =
        |b: u8| (keywords.is_some() && b == b'$') || (eol.is_some() && (b == b'\r' || b == b'\n'));

    let n = input.len();
    let mut out: Vec<u8> = Vec::with_capacity(n + n / 16);
    let mut cr_pending = false;
    let mut kwbuf: Vec<u8> = Vec::new();
    let mut next_sign = 0usize;
    let mut src_format: Option<&'static [u8]> = None;
    let mut p = 0usize;

    // `translate_newline`: writes the configured eol, enforcing source consistency.
    let mut newline = |out: &mut Vec<u8>, nl: &'static [u8]| -> Result<(), TranslateError> {
        match src_format {
            Some(first) => {
                if !params.repair && first != nl {
                    return Err(TranslateError::InconsistentEol);
                }
            }
            None => src_format = Some(nl),
        }
        if let Some(e) = eol {
            out.extend_from_slice(e);
        }
        Ok(())
    };

    while p < n {
        let c = input[p];
        if cr_pending {
            let nl: &'static [u8] = if c == b'\n' {
                p += 1;
                b"\r\n"
            } else {
                b"\r"
            };
            newline(&mut out, nl)?;
            cr_pending = false;
        } else if !kwbuf.is_empty() && c == b'$' {
            kwbuf.push(b'$');
            p += 1;
            let name = keywords
                .and_then(|k| match_keyword(&kwbuf, k))
                .map(<[u8]>::to_vec);
            if name.is_none() {
                // Reuse the closing '$' as the start of a new candidate.
                p -= 1;
                kwbuf.pop();
            }
            let done = match (&name, keywords) {
                (Some(name), Some(k)) => {
                    let value = if params.expand { k.get(name) } else { None };
                    subst_keyword(&mut kwbuf, name, value) || kwbuf.len() >= KEYWORD_MAX_LEN
                }
                _ => true,
            };
            if done {
                out.extend_from_slice(&kwbuf);
                next_sign = 0;
                kwbuf.clear();
            } else {
                if next_sign == 0 {
                    next_sign = kwbuf.len() - 1;
                }
                continue;
            }
        } else if kwbuf.len() == KEYWORD_MAX_LEN - 1
            || (!kwbuf.is_empty() && (c == b'\r' || c == b'\n'))
        {
            if next_sign > 0 {
                // Roll back to the last '$' seen and rescan from there.
                p -= kwbuf.len() - next_sign;
                kwbuf.truncate(next_sign);
                next_sign = 0;
            }
            out.extend_from_slice(&kwbuf);
            kwbuf.clear();
        } else if !kwbuf.is_empty() {
            kwbuf.push(c);
            p += 1;
            continue;
        }

        // Boring state: copy a run of uninteresting bytes, then enter an interesting state.
        let start = p;
        while p < n && !interesting(input[p]) {
            p += 1;
        }
        out.extend_from_slice(&input[start..p]);
        if p < n {
            match input[p] {
                b'$' => {
                    kwbuf.push(b'$');
                    p += 1;
                }
                b'\r' => {
                    cr_pending = true;
                    p += 1;
                }
                _ => {
                    p += 1;
                    newline(&mut out, b"\n")?;
                }
            }
        }
    }

    if cr_pending {
        newline(&mut out, b"\r")?;
    }
    out.extend_from_slice(&kwbuf);
    Ok(out)
}
