//! A throwaway Apache httpd + `mod_dav_svn` serving an `SVNParentPath` over plain HTTP on
//! `127.0.0.1`, anonymous read/write. Both the engine and the `svn` CLI talk to it, so crash
//! tests and benchmarks exercise the real wire protocol of the installed Subversion server.
//!
//! Binary: `OPENSVN_APACHE_BIN`, else `apache2` / `httpd` on `PATH` or in `/usr/sbin`. Modules:
//! `OPENSVN_APACHE_MODULES`, else the first of `/usr/lib/apache2/modules`,
//! `/usr/lib64/httpd/modules`, `/usr/libexec/apache2` that holds `mod_dav_svn.so`.

use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::{Error, Result, err};

/// A running Apache instance; stopped (and its directory removed) on drop.
pub struct Apache {
    child: Child,
    dir: tempfile::TempDir,
    port: u16,
}

fn find_binary() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("OPENSVN_APACHE_BIN") {
        return Some(PathBuf::from(p));
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
    dirs.push(PathBuf::from("/usr/sbin"));
    for name in ["apache2", "httpd"] {
        for d in &dirs {
            let p = d.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

fn find_modules() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("OPENSVN_APACHE_MODULES") {
        return Some(PathBuf::from(p));
    }
    [
        "/usr/lib/apache2/modules",
        "/usr/lib64/httpd/modules",
        "/usr/lib/httpd/modules",
        "/usr/libexec/apache2",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|d| d.join("mod_dav_svn.so").is_file())
}

/// True when the current user is root (Apache then drops to `www-data` / `apache`).
pub fn running_as_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

/// The unprivileged user Apache runs as when started by root.
fn service_user() -> &'static str {
    let has = |u: &str| {
        Command::new("id")
            .arg(u)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    };
    if has("www-data") { "www-data" } else { "apache" }
}

fn free_port() -> Result<u16> {
    let l = TcpListener::bind("127.0.0.1:0").map_err(|e| err(format!("bind: {e}")))?;
    let port = l
        .local_addr()
        .map_err(|e| err(format!("local_addr: {e}")))?
        .port();
    Ok(port)
}

impl Apache {
    /// Whether an Apache binary with `mod_dav_svn` was found.
    pub fn available() -> bool {
        find_binary().is_some() && find_modules().is_some()
    }

    /// Starts a server; repositories live in [`Apache::repos_dir`].
    pub fn start() -> Result<Self> {
        let bin = find_binary().ok_or_else(|| err("apache2/httpd not found"))?;
        let modules = find_modules().ok_or_else(|| err("mod_dav_svn.so not found"))?;
        let dir = tempfile::Builder::new()
            .prefix("opensvn-apache-")
            .tempdir()
            .map_err(|e| err(format!("tempdir: {e}")))?;
        let repos = dir.path().join("repos");
        std::fs::create_dir_all(&repos).map_err(|e| err(format!("mkdir: {e}")))?;
        let root = running_as_root();
        if root {
            // The service user must reach the repositories through the temp directory.
            chmod(dir.path(), "755")?;
        }
        let port = free_port()?;
        let m = |name: &str| format!("LoadModule {name}_module {}/mod_{name}.so", modules.display());
        let mut conf = String::new();
        // Debian builds the MPM and unixd as modules; other distributions compile some in.
        for (name, file) in [("mpm_event", "mod_mpm_event.so"), ("unixd", "mod_unixd.so")] {
            if modules.join(file).is_file() {
                conf.push_str(&format!(
                    "LoadModule {name}_module {}/{file}\n",
                    modules.display()
                ));
            }
        }
        for name in ["authz_core", "dav", "dav_svn"] {
            conf.push_str(&m(name));
            conf.push('\n');
        }
        let d = dir.path().display();
        conf.push_str(&format!(
            "ServerRoot {d}\nServerName 127.0.0.1\nListen 127.0.0.1:{port}\nPidFile {d}/httpd.pid\n\
             ErrorLog {d}/error.log\nLogLevel warn\nTimeout 300\nKeepAlive On\n\
             MaxKeepAliveRequests 0\nThreadsPerChild 25\nMaxRequestWorkers 100\n\
             SVNInMemoryCacheSize 65536\n\
             <Location /svn>\n  DAV svn\n  SVNParentPath {}\n  SVNListParentPath on\n\
             \x20 SVNCacheTextDeltas on\n  SVNCacheFullTexts on\n</Location>\n",
            repos.display()
        ));
        if root {
            let user = service_user();
            conf.push_str(&format!("User {user}\nGroup {user}\n"));
        }
        let conf_path = dir.path().join("httpd.conf");
        std::fs::write(&conf_path, conf).map_err(|e| err(format!("write conf: {e}")))?;
        let child = Command::new(&bin)
            .arg("-f")
            .arg(&conf_path)
            .arg("-DFOREGROUND")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| err(format!("spawn {}: {e}", bin.display())))?;
        let mut me = Self { child, dir, port };
        me.wait_ready()?;
        Ok(me)
    }

    fn wait_ready(&mut self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if let Ok(Some(status)) = self.child.try_wait() {
                let mut log = std::fs::read_to_string(self.dir.path().join("error.log"))
                    .unwrap_or_default();
                if let Some(mut e) = self.child.stderr.take() {
                    let _ = std::io::Read::read_to_string(&mut e, &mut log);
                }
                return Err(err(format!("apache exited ({status}): {log}")));
            }
            if std::net::TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Err(err("apache did not start listening within 20s"))
    }

    /// The `SVNParentPath` directory.
    pub fn repos_dir(&self) -> PathBuf {
        self.dir.path().join("repos")
    }

    /// URL of repository `name`.
    pub fn url(&self, name: &str) -> String {
        format!("http://127.0.0.1:{}/svn/{name}", self.port)
    }

    /// The server's error log (for failure messages).
    pub fn error_log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("error.log")).unwrap_or_default()
    }

    /// Makes repository `name` writable by the server process (needed when the tests run as
    /// root and Apache as the service user).
    pub fn hand_over(&self, name: &str) -> Result<()> {
        if !running_as_root() {
            return Ok(());
        }
        let user = service_user();
        let out = Command::new("chown")
            .arg("-R")
            .arg(format!("{user}:{user}"))
            .arg(self.repos_dir().join(name))
            .output()
            .map_err(|e| err(format!("chown: {e}")))?;
        if !out.status.success() {
            return Err(err(format!(
                "chown: {}",
                String::from_utf8_lossy(&out.stderr)
            )));
        }
        Ok(())
    }

    /// Copies repository `from` to `to` (same UUID, same history) and hands it over.
    pub fn copy_repo(&self, from: &str, to: &str) -> Result<()> {
        let out = Command::new("cp")
            .arg("-a")
            .arg(self.repos_dir().join(from))
            .arg(self.repos_dir().join(to))
            .output()
            .map_err(|e| err(format!("cp: {e}")))?;
        if !out.status.success() {
            return Err(err(format!("cp: {}", String::from_utf8_lossy(&out.stderr))));
        }
        self.hand_over(to)
    }
}

fn chmod(path: &Path, mode: &str) -> Result<()> {
    let ok = Command::new("chmod")
        .arg(mode)
        .arg(path)
        .status()
        .is_ok_and(|s| s.success());
    if ok {
        Ok(())
    } else {
        Err(Error(format!("chmod {mode} {}", path.display())))
    }
}

impl Drop for Apache {
    fn drop(&mut self) {
        // Graceful stop lets the worker processes exit; SIGKILL if it does not.
        let pid = self.child.id().to_string();
        let _ = Command::new("kill").args(["-TERM", &pid]).status();
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
