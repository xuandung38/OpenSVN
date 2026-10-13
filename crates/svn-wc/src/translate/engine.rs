//! Byte-exact incremental port of the libsvn_subr `translate_chunk` state machine (subst.c;
//! tags 1.14.5 and 1.15.0 are identical here).
//!
//! A [`Translator`] keeps svn's cross-chunk state (pending CR, keyword buffer, first source
//! line ending). svn's `next_sign_off` rollback marker is local to one `translate_chunk` call,
//! so it is reset on every [`Translator::push`]: feeding the same chunk sizes as svn therefore
//! reproduces its behaviour for malformed keywords that straddle a chunk boundary.

use super::error::TranslateError;
use super::keywords::Keywords;

/// Longest keyword string (`$...$`) that svn recognises (`SVN_KEYWORD_MAX_LEN`).
const KEYWORD_MAX_LEN: usize = 255;

/// Parameters of one translation run.
pub(crate) struct Params {
    /// End-of-line sequence to write; `None` leaves line endings untouched.
    pub eol: Option<&'static [u8]>,
    /// Convert inconsistent line endings instead of failing.
    pub repair: bool,
    /// Keywords to expand or contract; `None`/empty disables keyword handling.
    pub keywords: Option<Keywords>,
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

/// Index of the first `$` at or after `from` (the buffer always ends with one).
fn first_dollar(buf: &[u8], from: usize) -> usize {
    buf[from..]
        .iter()
        .position(|b| *b == b'$')
        .map_or(buf.len() - 1, |i| from + i)
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
            // svn pads with `while (*p != '$')`: the fill stops at the FIRST '$' after the start,
            // which may be an interior one when the scanner kept buffering past a failed candidate.
            None => {
                let end = first_dollar(buf, bp + 2);
                buf[bp + 2..end].fill(b' ');
            }
            Some(v) if v.len() <= max_value => {
                buf[bp + 3..bp + 3 + v.len()].copy_from_slice(v);
                let start = bp + 3 + v.len();
                let end = first_dollar(buf, start);
                buf[start..end].fill(b' ');
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

/// Incremental translator; see the module documentation.
pub(crate) struct Translator {
    params: Params,
    cr_pending: bool,
    kwbuf: Vec<u8>,
    src_format: Option<&'static [u8]>,
}

impl Translator {
    /// Creates a translator for one file.
    pub(crate) fn new(mut params: Params) -> Self {
        params.keywords = params.keywords.filter(|k| !k.is_empty());
        Self {
            params,
            cr_pending: false,
            kwbuf: Vec::new(),
            src_format: None,
        }
    }

    fn interesting(&self, b: u8) -> bool {
        (self.params.keywords.is_some() && b == b'$')
            || (self.params.eol.is_some() && (b == b'\r' || b == b'\n'))
    }

    /// `translate_newline`: writes the configured eol, enforcing source consistency.
    fn newline(&mut self, out: &mut Vec<u8>, nl: &'static [u8]) -> Result<(), TranslateError> {
        match self.src_format {
            Some(first) => {
                if !self.params.repair && first != nl {
                    return Err(TranslateError::InconsistentEol);
                }
            }
            None => self.src_format = Some(nl),
        }
        if let Some(e) = self.params.eol {
            out.extend_from_slice(e);
        }
        Ok(())
    }

    /// Translates one chunk (`translate_chunk` with a buffer), appending to `out`.
    pub(crate) fn push(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<(), TranslateError> {
        let n = input.len();
        let mut next_sign = 0usize;
        let mut p = 0usize;
        while p < n {
            let c = input[p];
            if self.cr_pending {
                let nl: &'static [u8] = if c == b'\n' {
                    p += 1;
                    b"\r\n"
                } else {
                    b"\r"
                };
                self.newline(out, nl)?;
                self.cr_pending = false;
            } else if !self.kwbuf.is_empty() && c == b'$' {
                self.kwbuf.push(b'$');
                p += 1;
                let name = self
                    .params
                    .keywords
                    .as_ref()
                    .and_then(|k| match_keyword(&self.kwbuf, k))
                    .map(<[u8]>::to_vec);
                if name.is_none() {
                    // Reuse the closing '$' as the start of a new candidate.
                    p -= 1;
                    self.kwbuf.pop();
                }
                let done = match (&name, &self.params.keywords) {
                    (Some(name), Some(k)) => {
                        let value = if self.params.expand {
                            k.get(name)
                        } else {
                            None
                        };
                        subst_keyword(&mut self.kwbuf, name, value)
                            || self.kwbuf.len() >= KEYWORD_MAX_LEN
                    }
                    _ => true,
                };
                if done {
                    out.extend_from_slice(&self.kwbuf);
                    next_sign = 0;
                    self.kwbuf.clear();
                } else {
                    if next_sign == 0 {
                        next_sign = self.kwbuf.len() - 1;
                    }
                    continue;
                }
            } else if self.kwbuf.len() == KEYWORD_MAX_LEN - 1
                || (!self.kwbuf.is_empty() && (c == b'\r' || c == b'\n'))
            {
                if next_sign > 0 {
                    // Roll back to the last '$' seen and rescan from there.
                    p -= self.kwbuf.len() - next_sign;
                    self.kwbuf.truncate(next_sign);
                    next_sign = 0;
                }
                out.extend_from_slice(&self.kwbuf);
                self.kwbuf.clear();
            } else if !self.kwbuf.is_empty() {
                self.kwbuf.push(c);
                p += 1;
                continue;
            }

            // Boring state: copy a run of uninteresting bytes, then enter an interesting state.
            let start = p;
            while p < n && !self.interesting(input[p]) {
                p += 1;
            }
            out.extend_from_slice(&input[start..p]);
            if p < n {
                match input[p] {
                    b'$' => {
                        self.kwbuf.push(b'$');
                        p += 1;
                    }
                    b'\r' => {
                        self.cr_pending = true;
                        p += 1;
                    }
                    _ => {
                        p += 1;
                        self.newline(out, b"\n")?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Flushes pending state (`translate_chunk` with a NULL buffer): a pending CR becomes an
    /// end-of-line, an open keyword candidate is written verbatim (no rollback).
    pub(crate) fn finish(mut self, out: &mut Vec<u8>) -> Result<(), TranslateError> {
        if self.cr_pending {
            self.newline(out, b"\r")?;
        }
        out.extend_from_slice(&self.kwbuf);
        Ok(())
    }
}
