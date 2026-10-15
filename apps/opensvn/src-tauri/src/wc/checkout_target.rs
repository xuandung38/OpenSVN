//! Where a checkout writes: the name of the new folder, the target folder itself and the default
//! parent folder (`general.checkoutRoot`).
//!
//! The parent always comes from a path grant (native dialog, or minted by Rust for the configured
//! checkout root); the WebView only supplies the name of one new child folder, which is checked
//! here so it can never step outside the parent.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use opensvn_ipc::AppError;
use svn_types::RelPath;

use crate::errors::invalid_argument;

/// Longest folder name accepted (bytes; the common file system limit).
const MAX_NAME_BYTES: usize = 255;

/// Characters Windows refuses in file names; refused everywhere so a working copy can be copied.
const FORBIDDEN: &[char] = &['<', '>', ':', '"', '|', '?', '*'];

/// Checks the name of the folder a checkout creates: exactly one path component that is valid on
/// every supported platform, that no Subversion client treats as an administrative directory
/// (`.svn`, `_svn` and their case, Unicode and trailing-dot aliases), and that contains no
/// invisible or bidirectional formatting characters (a name that displays as something else).
///
/// The friendly checks come first for specific messages; the name then has to pass the same
/// segment rules as a path from a server ([`RelPath::from_server`]), which is what refuses
/// reserved device names (`CON`, `NUL.txt`, `COM0`-`COM9`, `LPT¹`, ...).
///
/// Every refusal is `invalid_argument` with the refused name as `detail`. `checkout` runs this
/// before it redeems the grant, and the checkout dialog relies on that pair to keep the user's
/// parent folder instead of asking for it again.
pub fn validate_folder_name(name: &str) -> Result<&str, AppError> {
    let bad =
        |why: &str| invalid_argument(format!("Invalid folder name: {why}.")).with_detail(name);
    if name.is_empty() {
        return Err(bad("enter a name"));
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(bad("the name is too long"));
    }
    if name.contains(['/', '\\']) {
        return Err(bad("the name must not contain / or \\"));
    }
    if name == "." || name == ".." {
        return Err(bad("\".\" and \"..\" are not folder names"));
    }
    if name.chars().any(crate::display_text::is_deceptive) {
        return Err(bad(
            "the name contains an invisible or text-direction character",
        ));
    }
    if name.chars().any(char::is_control) || name.contains(FORBIDDEN) {
        return Err(bad(
            "the name contains a character that is not allowed (< > : \" | ? *)",
        ));
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(bad("the name must not end with a dot or a space"));
    }
    if name.starts_with(' ') {
        return Err(bad("the name must not start with a space"));
    }
    if svn_types::is_any_admin_dir_name(name) {
        return Err(bad(
            "the name is reserved for Subversion's administrative folder",
        ));
    }
    if RelPath::from_server(name).is_err() {
        // `from_server` refuses more than device names (stream separators, other platform
        // rules), so the message names no single cause.
        return Err(bad(
            "the name cannot be used for a folder on every supported system",
        ));
    }
    Ok(name)
}

/// The folder a checkout writes into.
#[derive(Debug)]
pub struct CheckoutTarget {
    /// Working copy root.
    pub path: PathBuf,
    /// The folder was created for this checkout (removed again when the checkout fails early).
    pub created: bool,
}

impl CheckoutTarget {
    /// After a failed or cancelled checkout: removes the folder this checkout created, but only
    /// while it is still empty. Never removes anything the user had before.
    pub fn discard_if_empty(&self) {
        if self.created && is_empty_dir(&self.path) {
            let _ = std::fs::remove_dir(&self.path);
        }
    }
}

fn is_empty_dir(path: &Path) -> bool {
    std::fs::read_dir(path).is_ok_and(|mut d| d.next().is_none())
}

/// Holds nothing but a `.svn` without `wc.db`: the skeleton of a checkout that was killed
/// before its working copy database was in place. The engine's working copy creation takes such
/// a skeleton over after checking it holds nothing else, and rejects it otherwise; nothing is
/// ever deleted here.
fn is_checkout_skeleton(path: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(path) else {
        return false;
    };
    let names: Vec<_> = entries.flatten().map(|e| e.file_name()).collect();
    let adm = path.join(".svn");
    names.len() == 1
        && names[0] == ".svn"
        && std::fs::symlink_metadata(&adm).is_ok_and(|m| m.is_dir())
        && !adm.join("wc.db").exists()
}

/// Refuses anything but a missing path or an empty, real folder.
fn check_existing(path: &Path, meta: &std::fs::Metadata) -> Result<(), AppError> {
    let shown = path.to_string_lossy().into_owned();
    if meta.file_type().is_symlink() {
        return Err(
            invalid_argument("The checkout folder is a link. Choose another name.")
                .with_path(shown),
        );
    }
    if !meta.is_dir() {
        return Err(
            invalid_argument("A file with this name already exists. Choose another name.")
                .with_path(shown),
        );
    }
    if !is_empty_dir(path) && !is_checkout_skeleton(path) {
        return Err(invalid_argument(
            "The checkout folder already exists and is not empty. Choose another name or an empty folder.",
        )
        .with_path(shown));
    }
    Ok(())
}

/// Resolves and prepares the checkout target.
///
/// With `name`, `dest` is the parent folder: the child folder is created (or reused when it exists
/// and is empty). Without `name`, `dest` itself becomes the working copy root and must be empty or
/// not exist yet.
pub fn prepare(dest: &Path, name: Option<&str>) -> Result<CheckoutTarget, AppError> {
    if !dest.is_absolute() {
        return Err(invalid_argument(
            "The checkout folder must be an absolute path.",
        ));
    }
    let Some(name) = name else {
        return match std::fs::symlink_metadata(dest) {
            Ok(meta) => {
                check_existing(dest, &meta)?;
                Ok(CheckoutTarget {
                    path: dest.to_path_buf(),
                    created: false,
                })
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(CheckoutTarget {
                path: dest.to_path_buf(),
                created: false,
            }),
            Err(e) => Err(io_error(dest, &e)),
        };
    };
    let name = validate_folder_name(name)?;
    if !dest.is_dir() {
        return Err(
            invalid_argument("The parent folder no longer exists. Choose it again.")
                .with_path(dest.to_string_lossy()),
        );
    }
    let path = dest.join(name);
    match std::fs::create_dir(&path) {
        Ok(()) => Ok(CheckoutTarget {
            path,
            created: true,
        }),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            let meta = std::fs::symlink_metadata(&path).map_err(|e| io_error(&path, &e))?;
            check_existing(&path, &meta)?;
            Ok(CheckoutTarget {
                path,
                created: false,
            })
        }
        Err(e) => Err(io_error(&path, &e)),
    }
}

fn io_error(path: &Path, e: &std::io::Error) -> AppError {
    invalid_argument("The checkout folder cannot be created.")
        .with_detail(e.to_string())
        .with_path(path.to_string_lossy())
}

/// The home folder of the current user.
fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// Expands the configured checkout root (`~` is the home folder) to an absolute path.
pub fn expand_root(root: &str) -> Result<PathBuf, AppError> {
    let rest = if root == "~" {
        Some("")
    } else {
        root.strip_prefix("~/").or_else(|| root.strip_prefix("~\\"))
    };
    let path = match rest {
        Some(rest) => {
            let home = home_dir().ok_or_else(|| invalid_argument("The home folder is unknown."))?;
            if rest.is_empty() {
                home
            } else {
                home.join(rest)
            }
        }
        None => PathBuf::from(root),
    };
    if !path.is_absolute() {
        return Err(invalid_argument(
            "The default checkout folder is not an absolute path. Choose it again in Settings.",
        ));
    }
    Ok(path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn folder_names_are_one_portable_component() {
        for ok in [
            "project",
            "my repo",
            "a.b",
            "trunk-2026",
            "Ünïcödé",
            ".hidden",
            "con-tools",
            "COM10",
            "svn",
            "my_svn",
            ".svnignore",
        ] {
            assert_eq!(validate_folder_name(ok).unwrap(), ok, "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            "a/b",
            "a\\b",
            "../x",
            "x/..",
            "/abs",
            "C:",
            "a:b",
            "a*b",
            "q?",
            "p|q",
            "<x>",
            "\"q\"",
            "tab\there",
            "nul\0x",
            "dot.",
            "space ",
            " lead",
            "CON",
            "con",
            "nul.txt",
            "Aux",
            "PRN.tar.gz",
            "COM1",
            "lpt9",
            "COM¹",
            "CONIN$",
            "COM0",
            "lpt0.log",
            "LPT²",
            "com³.txt",
            // administrative folder names and their aliases
            ".svn",
            ".SVN",
            "_svn",
            "_SVN",
            "_svn.",
            "_Svn ",
            "\u{FF3F}svn",
            "_s\u{200D}vn",
            "SVN~1",
            // invisible and bidirectional format characters (Cf)
            "a\u{202E}gnp.exe",
            "a\u{2066}b",
            "a\u{2069}b",
            "zero\u{200B}width",
            "rlm\u{200F}",
            "bom\u{FEFF}",
            "soft\u{00AD}hyphen",
            "tag\u{E0041}",
            "num\u{0600}sign",
            "num\u{0605}",
            "ayah\u{06DD}",
            "syriac\u{070F}",
            "beam\u{1D173}",
            "phrase\u{1D17A}",
            "line\u{2028}sep",
            "para\u{2029}sep",
            "hangul\u{115F}",
            "hangul\u{1160}",
            "hangul\u{3164}",
            "hangul\u{FFA0}",
        ] {
            assert!(
                validate_folder_name(bad).is_err(),
                "{bad:?} must be refused"
            );
        }
        assert!(validate_folder_name(&"a".repeat(256)).is_err());
        assert!(validate_folder_name(&"a".repeat(255)).is_ok());
    }

    #[test]
    fn creates_a_new_child_and_removes_it_only_while_empty() {
        let dir = tempfile::tempdir().unwrap();
        let t = prepare(dir.path(), Some("proj")).unwrap();
        assert!(t.created && t.path == dir.path().join("proj") && t.path.is_dir());
        std::fs::write(t.path.join("keep.txt"), b"x").unwrap();
        t.discard_if_empty();
        assert!(t.path.join("keep.txt").exists(), "never deletes content");
        std::fs::remove_file(t.path.join("keep.txt")).unwrap();
        t.discard_if_empty();
        assert!(!t.path.exists());
    }

    #[test]
    fn reuses_an_empty_folder_and_refuses_non_empty_files_and_links() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("empty")).unwrap();
        let t = prepare(dir.path(), Some("empty")).unwrap();
        assert!(!t.created);
        t.discard_if_empty();
        assert!(
            dir.path().join("empty").is_dir(),
            "a folder the user had stays"
        );

        std::fs::create_dir(dir.path().join("full")).unwrap();
        std::fs::write(dir.path().join("full/a"), b"x").unwrap();
        let err = prepare(dir.path(), Some("full")).unwrap_err();
        assert!(err.message.contains("not empty"), "{}", err.message);
        assert!(prepare(&dir.path().join("full"), None).is_err());

        std::fs::write(dir.path().join("file"), b"x").unwrap();
        assert!(prepare(dir.path(), Some("file")).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("empty"), dir.path().join("link")).unwrap();
            let err = prepare(dir.path(), Some("link")).unwrap_err();
            assert!(err.message.contains("link"), "{}", err.message);
        }
        assert!(prepare(Path::new("relative"), Some("x")).is_err());
        assert!(
            prepare(&dir.path().join("gone"), Some("x")).is_err(),
            "parent must exist"
        );
        // the folder itself as target: a missing folder is fine, an empty one too
        assert!(prepare(&dir.path().join("new"), None).is_ok());
        assert!(prepare(&dir.path().join("empty"), None).is_ok());
    }

    #[test]
    fn the_skeleton_of_a_killed_checkout_is_reused_but_a_working_copy_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let skel = dir.path().join("skel");
        std::fs::create_dir_all(skel.join(".svn/tmp")).unwrap();
        std::fs::write(skel.join(".svn/tmp/wc.db.creating.lock"), b"").unwrap();
        let t = prepare(dir.path(), Some("skel")).unwrap();
        assert!(!t.created);
        // A working copy database, or anything next to `.svn`, is refused.
        std::fs::write(skel.join(".svn/wc.db"), b"").unwrap();
        prepare(dir.path(), Some("skel")).unwrap_err();
        std::fs::remove_file(skel.join(".svn/wc.db")).unwrap();
        std::fs::write(skel.join("notes.txt"), b"mine").unwrap();
        prepare(dir.path(), Some("skel")).unwrap_err();
        assert_eq!(std::fs::read(skel.join("notes.txt")).unwrap(), b"mine");
    }

    #[test]
    fn checkout_root_expands_home_and_must_be_absolute() {
        let home = home_dir().expect("home in tests");
        assert_eq!(expand_root("~").unwrap(), home);
        assert_eq!(expand_root("~/svn").unwrap(), home.join("svn"));
        assert!(expand_root("relative/dir").is_err());
        assert!(expand_root("~user/svn").is_err());
        let abs = std::env::temp_dir();
        assert_eq!(expand_root(&abs.to_string_lossy()).unwrap(), abs);
    }
}
