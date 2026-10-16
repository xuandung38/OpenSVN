//! Linux: environment variables the process must have from its first instruction.
//!
//! glibc reads `MALLOC_ARENA_MAX` and WebKitGTK reads `WEBKIT_DISABLE_*` once, when the process
//! starts, and this crate may not call `setenv` or `mallopt` (`unsafe_code` is forbidden). So when
//! one of them is missing, [`relaunch_if_needed`] replaces the process image with the same binary,
//! the same arguments and the missing variables (`execve`: same pid, before any thread or window
//! exists). [`STARTED_ENV`] records which variables it added, which stops a second relaunch and
//! lets [`scrub`] keep them out of the programs the app starts for the user.
//!
//! - `MALLOC_ARENA_MAX=2`: glibc gives every thread that allocates concurrently its own arena (up
//!   to 8 per core) and keeps what is freed there. The live heap of a 30k-file checkout peaks at
//!   about 23 MB, yet the arenas held 207 MB by the end of the RSS e2e; with two arenas 83 MB
//!   (`benches/results/2026-10-16-app-rss-linux.md`). jemalloc and mimalloc did less.
//! - With `appearance.hardwareAcceleration` off (or [`HW_ACCEL_ENV`]`=0`):
//!   `WEBKIT_DISABLE_DMABUF_RENDERER=1` and `WEBKIT_DISABLE_COMPOSITING_MODE=1`. WebKitGTK then
//!   paints in software: no GL, so on a machine without a GPU driver (llvmpipe) neither process
//!   loads LLVM or keeps software GL buffers (about 200 MB less in the measurements above); more
//!   CPU when large areas repaint. Some GPU drivers (NVIDIA on Wayland) show a blank window
//!   with acceleration, and turning it off is the usual fix there too. The default stays on until
//!   it is measured on machines with a GPU (#182).
//!
//! A variable the user set is never changed. macOS and Windows do nothing here.
// Only the relaunch (Linux) reads the setting; `scrub` runs everywhere and finds nothing to do.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::ffi::OsString;

/// `0`/`false`/`off`/`no` turns hardware acceleration off, `1`/`true`/`on`/`yes` turns it on,
/// whatever the setting says.
pub const HW_ACCEL_ENV: &str = "OPENSVN_HARDWARE_ACCELERATION";

/// Set by the relaunch to the comma-separated names of the variables it added.
pub const STARTED_ENV: &str = "OPENSVN_STARTED_ENV";

/// What [`HW_ACCEL_ENV`] decides, `None` when it is unset or not a known value.
fn env_choice(env: Option<&str>) -> Option<bool> {
    match env.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        Some("0" | "false" | "off" | "no") => Some(false),
        Some("1" | "true" | "on" | "yes") => Some(true),
        _ => None,
    }
}

/// The variables to add, given the current environment (`get`) and the stored setting (read only
/// when the environment does not decide).
pub fn missing(
    get: impl Fn(&str) -> Option<OsString>,
    stored_setting: impl FnOnce() -> Option<bool>,
) -> Vec<(&'static str, &'static str)> {
    if get(STARTED_ENV).is_some() {
        return Vec::new();
    }
    let mut add = Vec::new();
    if get("MALLOC_ARENA_MAX").is_none() {
        add.push(("MALLOC_ARENA_MAX", "2"));
    }
    let env = get(HW_ACCEL_ENV).and_then(|v| v.into_string().ok());
    let accelerated = match env_choice(env.as_deref()) {
        Some(choice) => choice,
        None => stored_setting().unwrap_or(true),
    };
    if !accelerated {
        for name in [
            "WEBKIT_DISABLE_DMABUF_RENDERER",
            "WEBKIT_DISABLE_COMPOSITING_MODE",
        ] {
            if get(name).is_none() {
                add.push((name, "1"));
            }
        }
    }
    add
}

/// Replaces the process with itself plus the [`missing`] variables. Returns when nothing is
/// missing, and when the relaunch fails (the app then starts as it is).
#[cfg(target_os = "linux")]
pub fn relaunch_if_needed(stored_setting: impl FnOnce() -> Option<bool>) {
    use std::os::unix::process::CommandExt;
    let add = missing(|name| std::env::var_os(name), stored_setting);
    if add.is_empty() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut args = std::env::args_os();
    let mut cmd = std::process::Command::new(exe);
    if let Some(arg0) = args.next() {
        cmd.arg0(arg0);
    }
    let names: Vec<&str> = add.iter().map(|(n, _)| *n).collect();
    let err = cmd
        .args(args)
        .envs(add.iter().copied())
        .env(STARTED_ENV, names.join(","))
        .exec();
    eprintln!("OpenSVN: starting without {}: {err}", names.join(", "));
}

/// Removes the variables the relaunch added from the environment of `cmd`, a program started for
/// the user (diff tool, editor, terminal, file opener): they are the app's tuning, not theirs.
pub fn scrub(cmd: &mut std::process::Command) {
    let Some(added) = std::env::var_os(STARTED_ENV) else {
        return;
    };
    for name in added.to_string_lossy().split(',').filter(|n| !n.is_empty()) {
        cmd.env_remove(name);
    }
    cmd.env_remove(STARTED_ENV);
}

#[cfg(test)]
#[allow(clippy::panic)]
mod tests {
    use std::collections::HashMap;
    use std::ffi::OsString;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<String, OsString> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn the_environment_decides_only_with_a_known_value() {
        assert_eq!(env_choice(None), None);
        for off in ["0", "false", "OFF", " no "] {
            assert_eq!(env_choice(Some(off)), Some(false), "{off}");
        }
        for on in ["1", "true", "On", "yes"] {
            assert_eq!(env_choice(Some(on)), Some(true), "{on}");
        }
        assert_eq!(env_choice(Some("maybe")), None);
        assert_eq!(env_choice(Some("")), None);
    }

    #[test]
    fn only_missing_variables_are_added_and_never_twice() {
        assert_eq!(missing(env(&[]), || None), [("MALLOC_ARENA_MAX", "2")]);
        assert_eq!(
            missing(env(&[("MALLOC_ARENA_MAX", "8")]), || Some(true)),
            []
        );
        assert_eq!(
            missing(env(&[("WEBKIT_DISABLE_DMABUF_RENDERER", "1")]), || Some(
                false
            )),
            [
                ("MALLOC_ARENA_MAX", "2"),
                ("WEBKIT_DISABLE_COMPOSITING_MODE", "1")
            ]
        );
        // Already relaunched: nothing, whatever is missing.
        assert_eq!(missing(env(&[(STARTED_ENV, "")]), || Some(false)), []);
    }

    #[test]
    fn the_stored_setting_is_read_only_when_the_environment_does_not_decide() {
        let off = missing(
            env(&[(HW_ACCEL_ENV, "0"), ("MALLOC_ARENA_MAX", "2")]),
            || panic!("setting read although the environment decides"),
        );
        assert_eq!(
            off,
            [
                ("WEBKIT_DISABLE_DMABUF_RENDERER", "1"),
                ("WEBKIT_DISABLE_COMPOSITING_MODE", "1")
            ]
        );
        let on = missing(
            env(&[(HW_ACCEL_ENV, "yes"), ("MALLOC_ARENA_MAX", "2")]),
            || panic!("setting read although the environment decides"),
        );
        assert!(on.is_empty());
        let stored_off = missing(env(&[("MALLOC_ARENA_MAX", "2")]), || Some(false));
        assert_eq!(stored_off.len(), 2);
    }
}
