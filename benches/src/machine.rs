//! What a benchmark report records about where it ran: CPU, memory, kernel, the filesystem the
//! working copies live on (type, mount options, whether it journals) and the server's version.

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

fn first_line(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty())
}

fn proc_field(file: &str, key: &str) -> Option<String> {
    std::fs::read_to_string(file).ok()?.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim() == key).then(|| v.trim().to_owned())
    })
}

/// `"<model> x <n> CPUs, <mem> GiB RAM, <kernel>"`.
pub fn machine() -> String {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    let model = proc_field("/proc/cpuinfo", "model name").unwrap_or_else(|| "unknown CPU".into());
    let mem = proc_field("/proc/meminfo", "MemTotal")
        .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        .map(|kib| format!("{:.1} GiB RAM", kib as f64 / (1 << 20) as f64))
        .unwrap_or_else(|| "unknown RAM".into());
    let kernel = first_line("uname", &["-sr"]).unwrap_or_else(|| "unknown kernel".into());
    format!("{model}, {cpus} CPUs, {mem}, {kernel}")
}

/// The filesystem holding `path`: type, device, mount options and whether it journals.
pub fn filesystem(path: &Path) -> String {
    let p = path.to_string_lossy();
    let Some(line) = first_line(
        "findmnt",
        &["-n", "-o", "FSTYPE,SOURCE,OPTIONS", "-T", p.as_ref()],
    ) else {
        return "unknown (findmnt failed)".into();
    };
    let mut parts = line.split_whitespace();
    let (fstype, source, options) = (
        parts.next().unwrap_or("?").to_owned(),
        parts.next().unwrap_or("?").to_owned(),
        parts.next().unwrap_or("").to_owned(),
    );
    let journal = match fstype.as_str() {
        "ext3" | "ext4" | "ext2" => ext_journal(&source, &options),
        "xfs" => "journaled (XFS metadata log)".into(),
        "btrfs" => "not journaled (copy-on-write, log tree for fsync)".into(),
        "f2fs" => "not journaled (log-structured, checkpoints)".into(),
        "tmpfs" => "not journaled (in memory)".into(),
        "overlay" => "overlay (journal of the upper layer unknown)".into(),
        _ => "journaling unknown".into(),
    };
    format!("{fstype} on {source} ({options}); {journal}")
}

fn ext_journal(source: &str, options: &str) -> String {
    // A journaled ext3/ext4 filesystem has a jbd2 instance named `<device>-<inode>`; without one
    // it runs without a journal (mkfs `^has_journal`). No root tools needed.
    let dev = source.rsplit('/').next().unwrap_or(source);
    let jbd2 = std::fs::read_dir("/proc/fs/jbd2").map(|rd| {
        rd.filter_map(|e| e.ok()).any(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with(&format!("{dev}-"))
        })
    });
    let data = std::fs::read_to_string(format!("/proc/fs/ext4/{dev}/options"))
        .ok()
        .and_then(|o| {
            o.lines()
                .find_map(|l| l.strip_prefix("data=").map(str::to_owned))
        })
        .or_else(|| {
            options
                .split(',')
                .find_map(|o| o.strip_prefix("data=").map(str::to_owned))
        });
    match (jbd2, data) {
        (Ok(true), d) => format!(
            "journaled (jbd2, data={})",
            d.as_deref().unwrap_or("ordered")
        ),
        (Ok(false), _) => "NOT journaled (no jbd2 journal for this device)".into(),
        (Err(_), Some(d)) => format!("journaled (data={d})"),
        (Err(_), None) => "journaling unknown".into(),
    }
}

/// The `Server:` header of the HTTP server at `url` (`http://host:port/...`), which names the
/// Apache and mod_dav_svn versions (`Apache/2.4.x (Unix) SVN/1.15.0`).
pub fn server(url: &str) -> String {
    server_header(url).unwrap_or_else(|| "unknown server".into())
}

fn server_header(url: &str) -> Option<String> {
    let rest = url.strip_prefix("http://")?;
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
    let mut s = TcpStream::connect(host).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(
        s,
        "OPTIONS /{path} HTTP/1.0\r\nHost: {host}\r\nContent-Length: 0\r\n\r\n"
    )
    .ok()?;
    let mut buf = Vec::new();
    let _ = s.take(64 * 1024).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.eq_ignore_ascii_case("server")
            .then(|| v.trim().to_owned())
    })
}

/// `OPENSVN_BENCH_WRAP=<program>` runs every timed command as `<program> <command> <args...>`
/// (a profiler such as `strace -f -c` behind a script), with the environment kept.
pub fn wrap(c: Command) -> Command {
    let Some(w) = std::env::var_os("OPENSVN_BENCH_WRAP") else {
        return c;
    };
    let mut out = Command::new(w);
    out.arg(c.get_program()).args(c.get_args());
    for (k, v) in c.get_envs() {
        match v {
            Some(v) => out.env(k, v),
            None => out.env_remove(k),
        };
    }
    if let Some(d) = c.get_current_dir() {
        out.current_dir(d);
    }
    out
}
