//! Package relationships (`_rels/*.rels`): where the main part and a workbook's sheets live.

use std::io::{Read, Seek};

use crate::ExtractError;
use crate::package::Package;
use crate::xml::{Ev, Pull, attr};

/// One internal relationship, its target resolved to a part name.
pub(crate) struct Rel {
    /// `Id` attribute.
    pub(crate) id: String,
    /// `Type` attribute (a URI).
    pub(crate) kind: String,
    /// Part name of the target, without a leading `/`.
    pub(crate) target: String,
}

/// Resolves `target` against the folder `base` (`""` for the package root).
fn resolve(base: &str, target: &str) -> String {
    let mut parts: Vec<&str> = if target.starts_with('/') {
        Vec::new()
    } else {
        base.split('/').filter(|s| !s.is_empty()).collect()
    };
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// Relationships of `part` (`""` for the package itself). External targets are left out.
pub(crate) fn of_part<R: Read + Seek>(
    pkg: &mut Package<R>,
    part: &str,
) -> Result<Vec<Rel>, ExtractError> {
    let (dir, name) = part.rsplit_once('/').unwrap_or(("", part));
    let rels_name = if dir.is_empty() {
        format!("_rels/{name}.rels")
    } else {
        format!("{dir}/_rels/{name}.rels")
    };
    let depth = pkg.max_depth();
    let rels = pkg.with_part(&rels_name, |src| {
        let mut pull = Pull::new(src, depth);
        let mut out = Vec::new();
        loop {
            match pull.next()? {
                Ev::Eof => return Ok(out),
                Ev::Start(n, attrs) if n == "Relationship" => {
                    if attr(&attrs, "TargetMode")
                        .is_some_and(|m| m.eq_ignore_ascii_case("External"))
                    {
                        continue;
                    }
                    if let (Some(id), Some(kind), Some(target)) = (
                        attr(&attrs, "Id"),
                        attr(&attrs, "Type"),
                        attr(&attrs, "Target"),
                    ) {
                        out.push(Rel {
                            id: id.to_owned(),
                            kind: kind.to_owned(),
                            target: resolve(dir, target),
                        });
                    }
                }
                _ => {}
            }
        }
    })?;
    Ok(rels.unwrap_or_default())
}

/// The main part of the package: `usual` when it exists, else the `officeDocument` target of the
/// package relationships.
pub(crate) fn main_part<R: Read + Seek>(
    pkg: &mut Package<R>,
    usual: &str,
) -> Result<String, ExtractError> {
    if pkg.has_part(usual) {
        return Ok(usual.to_owned());
    }
    of_part(pkg, "")?
        .into_iter()
        .find(|r| r.kind.ends_with("/officeDocument"))
        .map(|r| r.target)
        .ok_or_else(|| ExtractError::MissingPart(usual.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::resolve;

    #[test]
    fn targets_resolve_against_the_part_folder() {
        assert_eq!(
            resolve("xl", "worksheets/sheet1.xml"),
            "xl/worksheets/sheet1.xml"
        );
        assert_eq!(resolve("xl", "/xl/worksheets/s.xml"), "xl/worksheets/s.xml");
        assert_eq!(resolve("xl/a", "../b.xml"), "xl/b.xml");
        assert_eq!(resolve("", "word/document.xml"), "word/document.xml");
        assert_eq!(resolve("", "../../x.xml"), "x.xml");
    }
}
