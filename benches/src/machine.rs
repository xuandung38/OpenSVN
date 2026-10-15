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
    let out = Command::new("dumpe2fs").args(["-h", source]).output();
    let features = out.ok().and_then(|o| {
        String::from_utf8_lossy(&o.stdout).lines().find_map(|l| {
            l.strip_prefix("Filesystem features:")
                .map(|f| f.trim().to_owned())
        })
    });
    let data = options
        .split(',')
        .find_map(|o| o.strip_prefix("data="))
        .unwrap_or("ordered");
    match features {
        Some(f) if f.split_whitespace().any(|x| x == "has_journal") => {
            format!("journaled (has_journal, data={data})")
        }
        Some(_) => "not journaled (no has_journal feature)".into(),
        None => "journaling unknown (dumpe2fs unavailable)".into(),
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
