//! Text of an Excel workbook: a `=== Sheet: name ===` heading per sheet, in workbook order, then
//! one `name!A1: value` line per non-empty cell, followed by `  [=formula]` when the cell has a
//! formula. Values are shown as stored: shared strings resolved, booleans as `TRUE`/`FALSE`,
//! numbers and dates unformatted.

use std::collections::HashMap;
use std::io::{BufRead, Read, Seek};

use crate::package::Package;
use crate::xml::{Ev, Output, Pull, attr, one_line};
use crate::{ExtractError, rels};

/// A sheet listed in the workbook.
struct Sheet {
    name: String,
    rel_id: String,
    hidden: bool,
}

fn workbook_sheets(src: &mut dyn BufRead, depth: usize) -> Result<Vec<Sheet>, ExtractError> {
    let mut pull = Pull::new(src, depth);
    let mut out = Vec::new();
    loop {
        match pull.next()? {
            Ev::Eof => return Ok(out),
            Ev::Start(n, attrs) if n == "sheet" => out.push(Sheet {
                name: attr(&attrs, "name").unwrap_or("").to_owned(),
                rel_id: attr(&attrs, "id").unwrap_or("").to_owned(),
                hidden: attr(&attrs, "state").is_some_and(|s| s != "visible"),
            }),
            _ => {}
        }
    }
}

/// Collects text inside `<t>` elements, leaving out phonetic runs (`<rPh>`), until the end of
/// the element that was just started.
fn rich_text(pull: &mut Pull<'_>, budget: &mut usize) -> Result<String, ExtractError> {
    let mut text = String::new();
    let (mut depth, mut in_t, mut in_ph) = (1usize, 0usize, 0usize);
    while depth > 0 {
        match pull.next()? {
            Ev::Start(n, _) => {
                depth += 1;
                match n.as_str() {
                    "t" => in_t += 1,
                    "rPh" => in_ph += 1,
                    _ => {}
                }
            }
            Ev::End(n) => {
                depth -= 1;
                match n.as_str() {
                    "t" => in_t = in_t.saturating_sub(1),
                    "rPh" => in_ph = in_ph.saturating_sub(1),
                    _ => {}
                }
            }
            Ev::Text(t) if in_t > 0 && in_ph == 0 => {
                *budget = budget
                    .checked_sub(t.len())
                    .ok_or(ExtractError::LimitExceeded(
                        "the workbook has too much text",
                    ))?;
                text.push_str(&t);
            }
            Ev::Text(_) => {}
            Ev::Eof => return Err(ExtractError::BadXml("unexpected end of the part".into())),
        }
    }
    Ok(text)
}

fn shared_strings(
    src: &mut dyn BufRead,
    depth: usize,
    max_count: usize,
    mut budget: usize,
) -> Result<Vec<String>, ExtractError> {
    let mut pull = Pull::new(src, depth);
    let mut out = Vec::new();
    loop {
        match pull.next()? {
            Ev::Eof => return Ok(out),
            Ev::Start(n, _) if n == "si" => {
                if out.len() >= max_count {
                    return Err(ExtractError::LimitExceeded("too many shared strings"));
                }
                out.push(rich_text(&mut pull, &mut budget)?);
            }
            _ => {}
        }
    }
}

/// Column letters of a 1-based column number (`1` is `A`, `27` is `AA`).
fn column_name(mut col: u32) -> String {
    let mut out = Vec::new();
    while col > 0 {
        let rem = (col - 1) % 26;
        out.push(b'A' + rem as u8);
        col = (col - 1) / 26;
    }
    out.reverse();
    String::from_utf8_lossy(&out).into_owned()
}

/// Column number and row of a reference such as `AB12`.
fn parse_ref(r: &str) -> Option<(u32, u32)> {
    let split = r.find(|c: char| c.is_ascii_digit())?;
    let (letters, digits) = r.split_at(split);
    if letters.is_empty() || letters.len() > 3 || !letters.bytes().all(|b| b.is_ascii_alphabetic())
    {
        return None;
    }
    let col = letters.bytes().fold(0u32, |acc, b| {
        acc * 26 + u32::from(b.to_ascii_uppercase() - b'A' + 1)
    });
    Some((col, digits.parse().ok()?))
}

/// Writes the cells of one worksheet part.
fn sheet_cells(
    src: &mut dyn BufRead,
    depth: usize,
    sheet: &str,
    strings: &[String],
    out: &mut Output,
) -> Result<(), ExtractError> {
    let mut pull = Pull::new(src, depth);
    let (mut row, mut col) = (0u32, 0u32);
    loop {
        match pull.next()? {
            Ev::Eof => return Ok(()),
            Ev::Start(n, attrs) if n == "row" => {
                row = attr(&attrs, "r")
                    .and_then(|r| r.parse().ok())
                    .unwrap_or(row + 1);
                col = 0;
            }
            Ev::Start(n, attrs) if n == "c" => {
                match attr(&attrs, "r").and_then(parse_ref) {
                    Some((c, r)) => (col, row) = (c, r),
                    None => col += 1,
                }
                let kind = attr(&attrs, "t").unwrap_or("n").to_owned();
                let cell = cell_content(&mut pull, out.room())?;
                let value = match kind.as_str() {
                    "s" => cell
                        .value
                        .trim()
                        .parse::<usize>()
                        .ok()
                        .and_then(|i| strings.get(i).cloned())
                        .unwrap_or_default(),
                    "inlineStr" => cell.inline,
                    "b" => match cell.value.trim() {
                        "1" => "TRUE".to_owned(),
                        "0" => "FALSE".to_owned(),
                        other => other.to_owned(),
                    },
                    _ => cell.value,
                };
                if value.is_empty() && cell.formula.is_empty() {
                    continue;
                }
                let mut line = format!("{sheet}!{}{row}: {}", column_name(col), one_line(&value));
                if !cell.formula.is_empty() {
                    line.push_str(&format!("  [={}]", one_line(&cell.formula)));
                }
                out.line(&line)?;
            }
            _ => {}
        }
    }
}

/// What a `<c>` element holds.
struct Cell {
    value: String,
    formula: String,
    inline: String,
}

/// Reads the children of the `<c>` element that was just started.
fn cell_content(pull: &mut Pull<'_>, room: usize) -> Result<Cell, ExtractError> {
    let mut cell = Cell {
        value: String::new(),
        formula: String::new(),
        inline: String::new(),
    };
    let mut budget = room;
    let mut depth = 1usize;
    // Which child the text belongs to.
    let mut target: Option<bool> = None;
    while depth > 0 {
        match pull.next()? {
            Ev::Start(n, _) => match n.as_str() {
                "is" => cell.inline = rich_text(pull, &mut budget)?,
                "v" => {
                    depth += 1;
                    target = Some(true);
                }
                "f" => {
                    depth += 1;
                    target = Some(false);
                }
                _ => depth += 1,
            },
            Ev::End(_) => {
                depth -= 1;
                target = None;
            }
            Ev::Text(t) => {
                budget = budget
                    .checked_sub(t.len())
                    .ok_or(ExtractError::LimitExceeded(
                        "the extracted text is too long",
                    ))?;
                match target {
                    Some(true) => cell.value.push_str(&t),
                    Some(false) => cell.formula.push_str(&t),
                    None => {}
                }
            }
            Ev::Eof => return Err(ExtractError::BadXml("unexpected end of the part".into())),
        }
    }
    Ok(cell)
}

/// Writes the text of every sheet of the workbook.
pub(crate) fn extract<R: Read + Seek>(
    pkg: &mut Package<R>,
    out: &mut Output,
) -> Result<(), ExtractError> {
    let depth = pkg.max_depth();
    let main = rels::main_part(pkg, "xl/workbook.xml")?;
    let dir = main.rsplit_once('/').map_or("", |(d, _)| d).to_owned();
    let sheets = pkg
        .with_part(&main, |src| workbook_sheets(src, depth))?
        .ok_or_else(|| ExtractError::MissingPart(main.clone()))?;
    let rels = rels::of_part(pkg, &main)?;
    let targets: HashMap<&str, &str> = rels
        .iter()
        .map(|r| (r.id.as_str(), r.target.as_str()))
        .collect();
    let strings_part = rels
        .iter()
        .find(|r| r.kind.ends_with("/sharedStrings"))
        .map_or_else(|| format!("{dir}/sharedStrings.xml"), |r| r.target.clone());
    let (max_strings, budget) = (pkg.max_shared_strings(), out.room());
    let strings = pkg
        .with_part(&strings_part, |src| {
            shared_strings(src, depth, max_strings, budget)
        })?
        .unwrap_or_default();
    for (i, sheet) in sheets.iter().enumerate() {
        let hidden = if sheet.hidden { " (hidden)" } else { "" };
        out.line(&format!("=== Sheet: {}{hidden} ===", one_line(&sheet.name)))?;
        let part = targets.get(sheet.rel_id.as_str()).map_or_else(
            || format!("{dir}/worksheets/sheet{}.xml", i + 1),
            |t| (*t).to_owned(),
        );
        let name = one_line(&sheet.name);
        pkg.with_part(&part, |src| sheet_cells(src, depth, &name, &strings, out))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{column_name, parse_ref};

    #[test]
    fn column_names_and_references() {
        assert_eq!(column_name(1), "A");
        assert_eq!(column_name(26), "Z");
        assert_eq!(column_name(27), "AA");
        assert_eq!(column_name(16_384), "XFD");
        assert_eq!(parse_ref("AB12"), Some((28, 12)));
        assert_eq!(parse_ref("12"), None);
        assert_eq!(parse_ref("A"), None);
    }
}
