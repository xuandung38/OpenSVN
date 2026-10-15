//! Text of a Word document.
//!
//! Every paragraph with text becomes one line (a manual line break starts a new line). A table
//! row becomes `| cell | cell |`, the paragraphs of a cell joined with ` / `; a table nested in a
//! cell is folded into that cell. Text boxes come out as their own lines. Deleted text of tracked
//! changes, field codes and the fallback copies of alternate content are left out.

use std::io::{Read, Seek};

use crate::package::Package;
use crate::xml::{Ev, Output, Pull};
use crate::{ExtractError, rels};

/// Open containers while walking the body.
enum Ctx {
    Para(String),
    Table(Vec<String>),
    Cell(String),
}

/// Appends `text` to a cell, separating it from earlier content.
fn append(cell: &mut String, text: &str) {
    if text.is_empty() {
        return;
    }
    if !cell.is_empty() {
        cell.push_str(" / ");
    }
    cell.push_str(text);
}

/// Walks one part (`document.xml`, a header, footnotes, ...) and writes its lines.
fn part_text(
    src: &mut dyn std::io::BufRead,
    max_depth: usize,
    out: &mut Output,
) -> Result<(), ExtractError> {
    let mut pull = Pull::new(src, max_depth);
    let mut stack: Vec<Ctx> = Vec::new();
    // Inside `<w:t>` (text is only taken there) and inside a run (tabs and breaks count there;
    // `<w:tab>` also defines tab stops in paragraph properties).
    let (mut in_t, mut in_run) = (0usize, 0usize);
    // Text gathered but not written yet is bounded by the room left in the output.
    let mut pending = 0usize;
    loop {
        match pull.next()? {
            Ev::Eof => return Ok(()),
            Ev::Start(name, _) => match name.as_str() {
                "p" => stack.push(Ctx::Para(String::new())),
                "tbl" => stack.push(Ctx::Table(Vec::new())),
                "tc" => stack.push(Ctx::Cell(String::new())),
                "r" => in_run += 1,
                "t" => in_t += 1,
                "Fallback" | "delText" | "instrText" => pull.skip()?,
                "tab" | "ptab" if in_run > 0 => push(&mut stack, "\t"),
                "br" | "cr" if in_run > 0 => push(&mut stack, "\n"),
                "noBreakHyphen" if in_run > 0 => push(&mut stack, "-"),
                _ => {}
            },
            Ev::Text(t) => {
                if in_t > 0 {
                    pending = pending.saturating_add(t.len());
                    if pending > out.room() {
                        return Err(ExtractError::LimitExceeded(
                            "the extracted text is too long",
                        ));
                    }
                    push(&mut stack, &t);
                }
            }
            Ev::End(name) => match name.as_str() {
                "r" => in_run = in_run.saturating_sub(1),
                "t" => in_t = in_t.saturating_sub(1),
                "p" => {
                    if let Some(Ctx::Para(text)) = stack.pop() {
                        deliver(&mut stack, &text, out)?;
                        if stack.is_empty() {
                            pending = 0;
                        }
                    }
                }
                "tc" => {
                    if let Some(Ctx::Cell(text)) = stack.pop()
                        && let Some(Ctx::Table(row)) = stack.last_mut()
                    {
                        row.push(text.replace('\t', " "));
                    }
                }
                "tr" => {
                    if let Some(Ctx::Table(row)) = stack.last_mut() {
                        let cells = std::mem::take(row);
                        if cells.iter().any(|c| !c.is_empty()) {
                            let line = format!("| {} |", cells.join(" | "));
                            // A row of a nested table belongs to the enclosing cell.
                            let n = stack.len();
                            match stack.get_mut(n.wrapping_sub(2)) {
                                Some(Ctx::Cell(cell)) => append(cell, &line),
                                _ => out.line(&line)?,
                            }
                        }
                    }
                }
                "tbl" => {
                    if matches!(stack.last(), Some(Ctx::Table(_))) {
                        stack.pop();
                    }
                }
                _ => {}
            },
        }
    }
}

/// Adds run text to the innermost open paragraph.
fn push(stack: &mut [Ctx], text: &str) {
    if let Some(Ctx::Para(p)) = stack.last_mut() {
        p.push_str(text);
    }
}

/// Hands a finished paragraph to its container: a table cell, or the output.
fn deliver(stack: &mut [Ctx], text: &str, out: &mut Output) -> Result<(), ExtractError> {
    match stack.last_mut() {
        Some(Ctx::Cell(cell)) => {
            for line in text.split('\n') {
                append(cell, line.trim_end());
            }
            Ok(())
        }
        _ => {
            for line in text.split('\n') {
                let line = line.trim_end();
                if !line.is_empty() {
                    out.line(line)?;
                }
            }
            Ok(())
        }
    }
}

/// Sort key that orders `header2.xml` before `header10.xml`.
fn natural_key(name: &str) -> (String, u64) {
    let stem = name.trim_end_matches(".xml");
    let digits = stem.len() - stem.trim_end_matches(|c: char| c.is_ascii_digit()).len();
    let (head, num) = stem.split_at(stem.len() - digits);
    (head.to_ascii_lowercase(), num.parse().unwrap_or(0))
}

/// Writes the text of the main document, then its headers, footers, notes and comments.
pub(crate) fn extract<R: Read + Seek>(
    pkg: &mut Package<R>,
    out: &mut Output,
) -> Result<(), ExtractError> {
    let depth = pkg.max_depth();
    let main = rels::main_part(pkg, "word/document.xml")?;
    let dir = main.rsplit_once('/').map_or("", |(d, _)| d).to_owned();
    pkg.with_part(&main, |src| part_text(src, depth, out))?
        .ok_or_else(|| ExtractError::MissingPart(main.clone()))?;

    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    let mut extra: Vec<(usize, (String, u64), String)> = pkg
        .names()
        .into_iter()
        .filter_map(|n| {
            let rest = n.strip_prefix(&prefix)?;
            if rest.contains('/') || !rest.to_ascii_lowercase().ends_with(".xml") {
                return None;
            }
            let lower = rest.to_ascii_lowercase();
            let rank = ["header", "footer", "footnotes", "endnotes", "comments"]
                .iter()
                .position(|k| {
                    lower.strip_prefix(k).is_some_and(|r| {
                        r.trim_end_matches(".xml")
                            .bytes()
                            .all(|b| b.is_ascii_digit())
                    })
                })?;
            Some((rank, natural_key(rest), n))
        })
        .collect();
    extra.sort();
    for (_, _, name) in extra {
        let title = name.rsplit('/').next().unwrap_or(&name).to_owned();
        let start = out.len();
        out.line(&format!("=== {title} ==="))?;
        let heading_len = out.len();
        pkg.with_part(&name, |src| part_text(src, depth, out))?;
        if out.len() == heading_len {
            // Nothing but the heading: drop it so empty parts leave no trace.
            out.truncate(start);
        }
    }
    Ok(())
}
