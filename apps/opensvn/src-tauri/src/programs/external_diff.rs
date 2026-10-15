//! Opening a diff in the external diff tool (`diff.externalTool`).
//!
//! The two sides are written as read-only copies (`left.<ext>` / `right.<ext>`) into a fresh
//! folder under the private `external-diff` directory and the configured program is started directly (never through a
//! shell) with `{left}` / `{right}` in its arguments replaced by those copies. When the arguments
//! name neither placeholder, the two paths are appended, which is what most tools expect.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use opensvn_ipc::AppError;

/// Placeholder replaced by the left (older) copy.
pub const LEFT_TOKEN: &str = "{left}";
/// Placeholder replaced by the right (newer) copy.
pub const RIGHT_TOKEN: &str = "{right}";
/// Staged copies older than this are removed on the next launch. External tools may outlive the
/// command (some return at once and keep the window open), so copies are never removed sooner.
pub const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// Starts a program. Tests substitute a recorder; the app uses [`NativeLauncher`].
pub trait ProgramLauncher: Send + Sync {
    /// Starts `program` with `args` and returns without waiting for it to exit.
    fn launch(&self, program: &Path, args: &[String]) -> Result<(), AppError>;
}

/// Launches through the operating system, without a shell.
pub struct NativeLauncher;

impl ProgramLauncher for NativeLauncher {
    fn launch(&self, program: &Path, args: &[String]) -> Result<(), AppError> {
        use std::process::{Command, Stdio};
        let is_mac_bundle = cfg!(target_os = "macos")
            && program
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("app"));
        let mut cmd = if is_mac_bundle {
            let mut c = Command::new("/usr/bin/open");
            c.arg("-n").arg("-a").arg(program).arg("--args");
            c
        } else {
            Command::new(program)
        };
        cmd.args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = cmd.spawn().map_err(|e| {
            crate::errors::invalid_argument("The external diff tool could not be started.")
                .with_detail(e.to_string())
        })?;
        // Reap the process when it exits so it never lingers as a zombie.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }
}

/// Arguments for the tool: placeholders replaced, or both paths appended when there are none.
pub fn expand_args(args: &[String], left: &str, right: &str) -> Vec<String> {
    let uses_tokens = args
        .iter()
        .any(|a| a.contains(LEFT_TOKEN) || a.contains(RIGHT_TOKEN));
    if !uses_tokens {
        let mut out = args.to_vec();
        out.push(left.to_owned());
        out.push(right.to_owned());
        return out;
    }
    args.iter()
        .map(|a| a.replace(LEFT_TOKEN, left).replace(RIGHT_TOKEN, right))
        .collect()
}

/// Longest extension kept on the staged copies.
pub const MAX_EXT_LEN: usize = 16;
/// Most staged folders kept at once; the oldest are removed first to make room.
pub const MAX_STAGED_DIRS: usize = 32;
/// Most bytes kept across all staged folders; the oldest are removed first to make room.
pub const MAX_STAGED_BYTES: u64 = 512 * 1024 * 1024;

/// Extension for the staged copies, taken from the last segment of `path` so tools pick the
/// right syntax. Only `[A-Za-z0-9]{1,16}` is kept; anything else (streams such as `::$DATA`,
/// spaces, shell metacharacters, non-ASCII) drops the extension entirely.
pub fn staged_extension(path: &str) -> Option<String> {
    let last = path.rsplit(['/', '\\']).find(|s| !s.is_empty())?;
    let (_, ext) = last.rsplit_once('.')?;
    let ok =
        (1..=MAX_EXT_LEN).contains(&ext.len()) && ext.bytes().all(|b| b.is_ascii_alphanumeric());
    ok.then(|| ext.to_owned())
}

/// Fixed names of the staged copies: `left.<ext>` / `right.<ext>` (or bare `left` / `right`).
/// Nothing from the repository reaches the file name except a sanitized extension, so device
/// names (`NUL .txt`), drive letters, UNC paths, alternate data streams and `cmd` metacharacters
/// cannot appear in it.
pub fn staged_names(path: &str) -> (String, String) {
    match staged_extension(path) {
        Some(ext) => (format!("left.{ext}"), format!("right.{ext}")),
        None => ("left".to_owned(), "right".to_owned()),
    }
}

fn io_error(what: &str, e: &std::io::Error) -> AppError {
    AppError::internal(format!(
        "Could not prepare the files for the external diff tool ({what})."
    ))
    .with_detail(e.to_string())
}

fn write_copy(dir: &Path, name: &str, data: &[u8]) -> Result<PathBuf, AppError> {
    let path = dir.join(name);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| io_error("file", &e))?;
    f.write_all(data).map_err(|e| io_error("write", &e))?;
    drop(f);
    make_read_only(&path)?;
    Ok(path)
}

/// Marks a staged copy read-only: edits in the tool must not look like they reach the working
/// copy.
pub fn make_read_only(path: &Path) -> Result<(), AppError> {
    let mut perms = std::fs::metadata(path)
        .map_err(|e| io_error("metadata", &e))?
        .permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(path, perms).map_err(|e| io_error("permissions", &e))
}

/// Creates a new, empty staged folder in `root` for copies written by someone else (the engine
/// streams them in), after removing the oldest folders beyond the caps. The size of the new
/// copies is not known yet: the byte cap is enforced again by the next staging.
pub fn new_stage_dir(root: &Path) -> Result<PathBuf, AppError> {
    let _guard = STAGING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    make_room(root, MAX_STAGED_DIRS.saturating_sub(1), MAX_STAGED_BYTES);
    let dir = root.join(uuid::Uuid::now_v7().simple().to_string());
    std::fs::create_dir(&dir).map_err(|e| io_error("folder", &e))?;
    Ok(dir)
}

/// Serializes staging so the folder and byte caps hold when launches overlap.
static STAGING: Mutex<()> = Mutex::new(());

/// Writes both sides into a new folder in `root` (the private external-diff directory) as
/// `left.<ext>` / `right.<ext>` (see [`staged_names`]) and returns their paths. A missing side
/// (added or deleted file) is staged as an empty file. Older folders are removed first, oldest
/// first, so at most [`MAX_STAGED_DIRS`] folders and [`MAX_STAGED_BYTES`] bytes stay staged.
pub fn stage_sides(
    root: &Path,
    path: &str,
    left: &[u8],
    right: &[u8],
) -> Result<(PathBuf, PathBuf), AppError> {
    stage_sides_capped(root, path, left, right, MAX_STAGED_DIRS, MAX_STAGED_BYTES)
}

fn stage_sides_capped(
    root: &Path,
    path: &str,
    left: &[u8],
    right: &[u8],
    max_dirs: usize,
    max_bytes: u64,
) -> Result<(PathBuf, PathBuf), AppError> {
    let incoming = (left.len() + right.len()) as u64;
    if incoming > max_bytes {
        return Err(crate::errors::invalid_argument(
            "The file is too large to open in the external diff tool.",
        ));
    }
    let _guard = STAGING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    make_room(root, max_dirs.saturating_sub(1), max_bytes - incoming);
    let session = root.join(uuid::Uuid::now_v7().simple().to_string());
    std::fs::create_dir(&session).map_err(|e| io_error("folder", &e))?;
    let (ln, rn) = staged_names(path);
    let l = write_copy(&session, &ln, left)?;
    let r = write_copy(&session, &rn, right)?;
    Ok((l, r))
}

/// Staged folders in `root`, oldest first (names are UUIDv7, which sort by creation time).
/// Symbolic links and plain files are never treated as staged folders.
fn staged_dirs(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs
}

/// Removes the oldest staged folders until at most `keep_dirs` remain and they hold at most
/// `keep_bytes` bytes.
fn make_room(root: &Path, keep_dirs: usize, keep_bytes: u64) {
    let dirs: Vec<(PathBuf, u64)> = staged_dirs(root)
        .into_iter()
        .map(|d| {
            let size = tree_size(&d);
            (d, size)
        })
        .collect();
    let mut total: u64 = dirs.iter().map(|(_, s)| s).sum();
    let mut count = dirs.len();
    for (dir, size) in dirs {
        if count <= keep_dirs && total <= keep_bytes {
            break;
        }
        remove_tree(&dir);
        count -= 1;
        total -= size;
    }
}

/// Calls `f` for every regular file under `dir`, never following symbolic links.
fn walk_files(dir: &Path, f: &mut dyn FnMut(&Path, &std::fs::Metadata)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        let kind = meta.file_type();
        if kind.is_dir() {
            walk_files(&path, f);
        } else if kind.is_file() {
            f(&path, &meta);
        }
    }
}

fn tree_size(dir: &Path) -> u64 {
    let mut total = 0;
    walk_files(dir, &mut |_, m| total += m.len());
    total
}

/// Best effort: removes staged folders in `root` last modified more than `max_age` ago.
pub fn sweep_stale(root: &Path, max_age: Duration) {
    let now = SystemTime::now();
    for dir in staged_dirs(root) {
        let old = std::fs::symlink_metadata(&dir)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age > max_age);
        if old {
            remove_tree(&dir);
        }
    }
}

/// Removes a staged folder without following symbolic links: a link inside it is removed, never
/// its target. On Windows the read-only copies block removal, so the flag is cleared first, on
/// regular files only; Unix removal depends on the folder's permissions alone, so nothing is
/// changed there.
fn remove_tree(dir: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return;
    };
    if !meta.file_type().is_dir() {
        return;
    }
    #[cfg(windows)]
    walk_files(dir, &mut |path, meta| {
        let mut perms = meta.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        let _ = std::fs::set_permissions(path, perms);
    });
    // `remove_dir_all` removes symbolic links themselves and does not descend into them.
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_owned()).collect()
    }

    #[test]
    fn placeholders_are_replaced_inside_arguments() {
        assert_eq!(
            expand_args(&s(&["--diff", "--left={left}", "{right}"]), "/a", "/b"),
            s(&["--diff", "--left=/a", "/b"])
        );
    }

    #[test]
    fn without_placeholders_both_paths_are_appended() {
        assert_eq!(expand_args(&s(&["-w"]), "/a", "/b"), s(&["-w", "/a", "/b"]));
        assert_eq!(expand_args(&[], "/a", "/b"), s(&["/a", "/b"]));
    }

    #[test]
    fn staged_names_are_fixed_and_keep_only_a_safe_extension() {
        let n = |p: &str| staged_names(p);
        let txt = ("left.txt".to_owned(), "right.txt".to_owned());
        let bare = ("left".to_owned(), "right".to_owned());
        assert_eq!(
            n("src/engine/renderer.ts"),
            ("left.ts".into(), "right.ts".into())
        );
        assert_eq!(n("a/b/"), bare);
        assert_eq!(n(""), bare);
        assert_eq!(n("/.."), bare);
        // Drive letters, UNC paths and device names never reach the file name.
        assert_eq!(n(r"C:\x\a.txt"), txt);
        assert_eq!(n(r"\\srv\share\a.txt"), txt);
        assert_eq!(n("dir/NUL .txt"), txt);
        assert_eq!(n("COM1 .txt"), txt);
        assert_eq!(n("CONIN$"), bare);
        assert_eq!(n("COM1"), bare);
        // Alternate data streams, shell metacharacters, bidi and over-long extensions are dropped.
        assert_eq!(n("a.txt::$DATA"), bare);
        assert_eq!(n("a.txt:stream"), bare);
        assert_eq!(n("x&calc.b^a%t"), bare);
        assert_eq!(n("x.c(m)d"), bare);
        assert_eq!(
            n("x/..\u{202E}gpj.exe"),
            ("left.exe".into(), "right.exe".into())
        );
        assert_eq!(n("x.t xt"), bare);
        assert_eq!(n("x.abcdefghijklmnopq"), bare);
        assert_eq!(
            n("x.abcdefghijklmnop"),
            (
                "left.abcdefghijklmnop".into(),
                "right.abcdefghijklmnop".into()
            )
        );
        assert_eq!(n("x.tx\u{0442}"), bare);
        for p in [
            r"C:\x\a.txt",
            "a.txt::$DATA",
            "NUL .txt",
            "x&y.b%t",
            "..\\..\\evil.sh",
        ] {
            let (l, r) = n(p);
            for name in [l, r] {
                assert!(
                    name.chars().all(|c| c.is_ascii_alphanumeric() || c == '.'),
                    "{p} -> {name}"
                );
                assert!(name.starts_with("left") || name.starts_with("right"));
            }
        }
    }

    #[test]
    fn sides_are_staged_read_only_under_fixed_names() {
        let root = tempfile::tempdir().unwrap();
        let (l, r) = stage_sides(root.path(), "dir/NUL .txt", b"old\n", b"").unwrap();
        assert_eq!(std::fs::read(&l).unwrap(), b"old\n");
        assert_eq!(std::fs::read(&r).unwrap(), b"");
        assert_eq!(l.file_name().unwrap(), "left.txt");
        assert_eq!(r.file_name().unwrap(), "right.txt");
        assert_eq!(l.parent(), r.parent());
        assert!(l.starts_with(root.path()) && r.starts_with(root.path()));
        assert!(std::fs::metadata(&l).unwrap().permissions().readonly());
        // A second launch never reuses or overwrites the first copies.
        let (l2, _) = stage_sides(root.path(), "a.txt", b"new\n", b"").unwrap();
        assert_ne!(l, l2);
        assert_eq!(std::fs::read(&l).unwrap(), b"old\n");
    }

    #[test]
    fn staging_keeps_at_most_the_folder_cap_removing_the_oldest() {
        let root = tempfile::tempdir().unwrap();
        let mut firsts = Vec::new();
        for i in 0..5 {
            let (l, _) = stage_sides_capped(
                root.path(),
                "a.txt",
                format!("{i}").as_bytes(),
                b"",
                3,
                1 << 20,
            )
            .unwrap();
            firsts.push(l);
        }
        assert_eq!(staged_dirs(root.path()).len(), 3);
        assert!(!firsts[0].exists() && !firsts[1].exists());
        assert!(firsts[2].exists() && firsts[4].exists());
    }

    #[test]
    fn staging_keeps_at_most_the_byte_cap_removing_the_oldest() {
        let root = tempfile::tempdir().unwrap();
        let kib = vec![b'x'; 1024];
        let (a, _) = stage_sides_capped(root.path(), "a", &kib, &kib, 32, 5 * 1024).unwrap();
        let (b, _) = stage_sides_capped(root.path(), "b", &kib, &kib, 32, 5 * 1024).unwrap();
        // 4 KiB staged; 2 more KiB do not fit, so the oldest folder goes.
        let (c, _) = stage_sides_capped(root.path(), "c", &kib, &kib, 32, 5 * 1024).unwrap();
        assert!(!a.exists() && b.exists() && c.exists());
        assert!(tree_size(root.path()) <= 5 * 1024);
        // A single launch larger than the cap is refused and removes nothing.
        let big = vec![b'x'; 6 * 1024];
        assert!(stage_sides_capped(root.path(), "d", &big, b"", 32, 5 * 1024).is_err());
        assert!(b.exists() && c.exists());
    }
    #[test]
    fn sweep_removes_only_stale_folders() {
        let root = tempfile::tempdir().unwrap();
        let (l, _) = stage_sides(root.path(), "a.txt", b"x", b"y").unwrap();
        sweep_stale(root.path(), STALE_AFTER);
        assert!(l.exists());
        sweep_stale(root.path(), Duration::ZERO);
        std::thread::sleep(Duration::from_millis(20));
        sweep_stale(root.path(), Duration::ZERO);
        assert!(!l.exists());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    /// A symbolic link planted in a staged folder is removed, never followed: the target keeps
    /// its permissions and contents, and a linked folder's files are left alone.
    #[cfg(unix)]
    #[test]
    fn removal_never_follows_planted_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret");
        std::fs::write(&secret, b"keep").unwrap();
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();
        let (l, _) = stage_sides(root.path(), "a.txt", b"x", b"y").unwrap();
        let staged = l.parent().unwrap();
        symlink(&secret, staged.join("link")).unwrap();
        symlink(outside.path(), staged.join("linked-dir")).unwrap();
        // A link in the root itself is not a staged folder.
        symlink(
            outside.path(),
            root.path().join("ffffffffffffffffffffffffffffffff"),
        )
        .unwrap();

        sweep_stale(root.path(), Duration::ZERO);
        std::thread::sleep(Duration::from_millis(20));
        sweep_stale(root.path(), Duration::ZERO);
        make_room(root.path(), 0, 0);

        assert!(!staged.exists());
        assert_eq!(std::fs::read(&secret).unwrap(), b"keep");
        let mode = std::fs::metadata(&secret).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(outside.path().exists());
        // The size walk does not follow links either.
        assert_eq!(tree_size(root.path()), 0);
    }
}
