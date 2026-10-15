//! The commit benchmark driver and its in-process engine side
//! (`compare-svn-commit engine <state> checkout|commit ...`).

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use opensvn_crash::engine::Engine;
use opensvn_crash::rng::Rng;
use opensvn_crash::server::Apache;
use opensvn_crash::svn::Svn;
use opensvn_crash::{Result, err};

struct Args {
    files: usize,
    modify_files: usize,
    big_mb: usize,
    reps: usize,
    out: PathBuf,
}

fn parse(argv: &[String]) -> Result<Args> {
    let mut a = Args {
        files: 10_000,
        modify_files: 1_000,
        big_mb: 40,
        reps: 3,
        out: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/bench-report/commit.md"),
    };
    let mut it = argv.iter();
    while let Some(k) = it.next() {
        let v = it.next().ok_or_else(|| err(format!("{k} needs a value")))?;
        let n = || v.parse::<usize>().map_err(|e| err(format!("{k} {v}: {e}")));
        match k.as_str() {
            "--files" => a.files = n()?,
            "--modify-files" => a.modify_files = n()?,
            "--big-mb" => a.big_mb = n()?,
            "--reps" => a.reps = n()?.max(1),
            "--out" => a.out = PathBuf::from(v),
            other => return Err(err(format!("unknown option {other}"))),
        }
    }
    a.modify_files = a.modify_files.min(a.files);
    Ok(a)
}

/// Entry point; returns the exit code.
pub fn main() -> i32 {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let result = if argv.first().map(String::as_str) == Some("engine") {
        engine_cmd(&argv[1..])
    } else {
        parse(&argv).and_then(|a| run(&a))
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("compare-svn-commit: {e}");
            1
        }
    }
}

/// `compare-svn-commit engine <state> checkout <url> <wc>` / `... commit <url> <wc> <msg>`.
fn engine_cmd(args: &[String]) -> Result<()> {
    let get = |i: usize| {
        args.get(i)
            .map(String::as_str)
            .ok_or_else(|| err("usage: compare-svn-commit engine <state> <op> <url> <wc> ..."))
    };
    let state = PathBuf::from(get(0)?);
    let (op, url, wc) = (get(1)?, get(2)?, PathBuf::from(get(3)?));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| err(format!("runtime: {e}")))?;
    rt.block_on(async {
        let engine = Engine::new(&state)?;
        let rev = match op {
            "checkout" => engine.checkout(url, &wc, None).await?,
            "commit" => engine.commit(url, &wc, get(4)?).await?,
            other => return Err(err(format!("unknown engine op {other}"))),
        };
        println!("rev={:?}", rev.map(|r| r.0));
        Ok(())
    })
}

/// A TCP pass-through in front of the server that counts the bytes clients send.
struct CountingProxy {
    port: u16,
    upload: Arc<AtomicU64>,
}

fn pump(mut from: TcpStream, mut to: TcpStream, count: Option<Arc<AtomicU64>>) {
    let mut buf = vec![0_u8; 64 * 1024];
    loop {
        match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if let Some(c) = &count {
                    c.fetch_add(n as u64, Ordering::Relaxed);
                }
                if to.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = to.shutdown(Shutdown::Write);
}

impl CountingProxy {
    fn start(upstream_port: u16) -> Result<Self> {
        let l = TcpListener::bind("127.0.0.1:0").map_err(|e| err(format!("bind: {e}")))?;
        let port = l
            .local_addr()
            .map_err(|e| err(format!("local_addr: {e}")))?
            .port();
        let upload = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&upload);
        std::thread::spawn(move || {
            for client in l.incoming().flatten() {
                let Ok(server) = TcpStream::connect(("127.0.0.1", upstream_port)) else {
                    continue;
                };
                let _ = client.set_nodelay(true);
                let _ = server.set_nodelay(true);
                let (Ok(c2), Ok(s2)) = (client.try_clone(), server.try_clone()) else {
                    continue;
                };
                let up = Arc::clone(&counter);
                std::thread::spawn(move || pump(client, server, Some(up)));
                std::thread::spawn(move || pump(s2, c2, None));
            }
        });
        Ok(Self { port, upload })
    }

    fn take(&self) -> u64 {
        self.upload.swap(0, Ordering::Relaxed)
    }
}

fn time(cmd: &mut Command, what: &str) -> Result<Duration> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let start = Instant::now();
    let out = cmd
        .output()
        .map_err(|e| err(format!("{what}: cannot run: {e}")))?;
    let el = start.elapsed();
    if std::env::var_os("OPENSVN_BENCH_STDERR").is_some() {
        eprint!("{}", String::from_utf8_lossy(&out.stderr));
    }
    if !out.status.success() {
        return Err(err(format!(
            "{what}: {}{}",
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        )));
    }
    Ok(el)
}

fn median<T: Copy + Ord + Default>(v: &[T]) -> T {
    let mut s = v.to_vec();
    s.sort();
    s.get(s.len() / 2).copied().unwrap_or_default()
}

const SEED: u64 = 0x0C0F_FEE5_EED5;

fn text_file(rng: &mut Rng, i: usize) -> Vec<u8> {
    let lines = 20 + rng.below(60) as usize;
    let mut s = String::with_capacity(lines * 40);
    for l in 0..lines {
        s.push_str(&format!(
            "file {i} line {l} value {:016x}\n",
            rng.next_u64()
        ));
    }
    s.into_bytes()
}

fn file_path(i: usize) -> String {
    format!("d{:02}/f{i:06}.txt", i % 100)
}

fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|e| err(format!("mkdir {}: {e}", p.display())))?;
    }
    std::fs::write(path, bytes).map_err(|e| err(format!("write {}: {e}", path.display())))
}

#[derive(Default)]
struct Row {
    name: String,
    svn: Vec<Duration>,
    engine: Vec<Duration>,
    svn_bytes: Vec<u64>,
    engine_bytes: Vec<u64>,
}

impl Row {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            ..Self::default()
        }
    }
    fn ratio(&self) -> f64 {
        median(&self.engine).as_secs_f64() / median(&self.svn).as_secs_f64().max(1e-9)
    }
}

struct Bench {
    svn: Svn,
    apache: Apache,
    proxy: CountingProxy,
    exe: PathBuf,
    scratch: PathBuf,
}

/// Who commits: the svn CLI or the engine.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Who {
    Svn,
    Engine,
}

impl Bench {
    fn url(&self, repo: &str) -> String {
        format!("http://127.0.0.1:{}/svn/{repo}", self.proxy.port)
    }

    fn engine(&self, args: &[&str]) -> Command {
        let mut c = Command::new(&self.exe);
        c.arg("engine")
            .arg(self.scratch.join("engine-state"))
            .args(args);
        c
    }

    /// A fresh repository `name` and a checkout of it by `who` at `wc`.
    fn fresh(&self, who: Who, name: &str, wc: &Path) -> Result<()> {
        let repo = self.apache.repos_dir().join(name);
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(wc);
        self.svn.create_repo(&repo)?;
        self.apache.hand_over(name)?;
        let (url, w) = (self.url(name), wc.to_string_lossy().into_owned());
        match who {
            Who::Svn => time(self.svn.cmd().args(["checkout", "-q", &url, &w]), "svn co")?,
            Who::Engine => time(&mut self.engine(&["checkout", &url, &w]), "engine co")?,
        };
        Ok(())
    }

    /// One timed commit of `wc` by `who`; returns (time, bytes uploaded).
    fn commit(&self, who: Who, name: &str, wc: &Path, msg: &str) -> Result<(Duration, u64)> {
        let w = wc.to_string_lossy().into_owned();
        self.proxy.take();
        let d = match who {
            Who::Svn => time(
                self.svn.cmd().args(["commit", "-q", "-m", msg, &w]),
                "svn commit",
            )?,
            Who::Engine => time(
                &mut self.engine(&["commit", &self.url(name), &w, msg]),
                "engine commit",
            )?,
        };
        let bytes = self.proxy.take();
        let st = self.svn.run_in(wc, &["status", "-q"])?;
        if !st.trim().is_empty() {
            let head: Vec<&str> = st.lines().take(10).collect();
            return Err(err(format!(
                "{msg}: not clean after commit:\n{}",
                head.join("\n")
            )));
        }
        Ok((d, bytes))
    }

    fn check_cat(&self, name: &str, rel: &str, expected: &[u8]) -> Result<()> {
        let mut c = self.svn.cmd();
        c.args(["cat", &format!("{}/{rel}", self.url(name))]);
        let out = c.output().map_err(|e| err(format!("svn cat: {e}")))?;
        if out.stdout != expected {
            return Err(err(format!("{name}/{rel}: repository content is wrong")));
        }
        Ok(())
    }

    fn add_many(&self, a: &Args, who: Who, row: &mut Row, rep: usize) -> Result<()> {
        let tag = if who == Who::Svn { "svn" } else { "eng" };
        let name = format!("add-{tag}-{rep}");
        let wc = self.scratch.join(format!("wc-{name}"));
        self.fresh(who, &name, &wc)?;
        let mut rng = Rng::new(SEED);
        for i in 0..a.files {
            write(&wc.join(file_path(i)), &text_file(&mut rng, i))?;
        }
        self.svn.run_in(&wc, &["add", "-q", "--force", "."])?;
        let (d, bytes) = self.commit(who, &name, &wc, "add")?;
        self.check_cat(
            &name,
            &file_path(a.files - 1),
            &std::fs::read(wc.join(file_path(a.files - 1))).map_err(|e| err(e.to_string()))?,
        )?;
        push(row, who, d, bytes);
        Ok(())
    }

    fn modify_many(&self, a: &Args, who: Who, row: &mut Row, rep: usize) -> Result<()> {
        let tag = if who == Who::Svn { "svn" } else { "eng" };
        let name = format!("add-{tag}-{rep}");
        let wc = self.scratch.join(format!("wc-{name}"));
        let step = (a.files / a.modify_files.max(1)).max(1);
        let mut last = (String::new(), Vec::new());
        for i in (0..a.files).step_by(step).take(a.modify_files) {
            let p = wc.join(file_path(i));
            let mut t = std::fs::read(&p).map_err(|e| err(format!("read: {e}")))?;
            t.extend_from_slice(format!("changed in rep {rep}\n").as_bytes());
            write(&p, &t)?;
            last = (file_path(i), t);
        }
        let (d, bytes) = self.commit(who, &name, &wc, "modify")?;
        self.check_cat(&name, &last.0, &last.1)?;
        push(row, who, d, bytes);
        Ok(())
    }

    fn big_one_byte(&self, a: &Args, who: Who, row: &mut Row, rep: usize) -> Result<()> {
        let tag = if who == Who::Svn { "svn" } else { "eng" };
        let name = format!("big-{tag}-{rep}");
        let wc = self.scratch.join(format!("wc-{name}"));
        self.fresh(who, &name, &wc)?;
        let mut bytes = Rng::new(SEED ^ 0xB16).bytes(a.big_mb << 20);
        write(&wc.join("big.bin"), &bytes)?;
        self.svn.run_in(&wc, &["add", "-q", "big.bin"])?;
        self.commit(who, &name, &wc, "big")?;
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0x5a;
        write(&wc.join("big.bin"), &bytes)?;
        let (d, sent) = self.commit(who, &name, &wc, "one byte")?;
        self.check_cat(&name, "big.bin", &bytes)?;
        push(row, who, d, sent);
        Ok(())
    }
}

fn push(row: &mut Row, who: Who, d: Duration, bytes: u64) {
    eprintln!(
        "{}: {} {:.2} s, {} bytes",
        row.name,
        if who == Who::Svn { "svn" } else { "engine" },
        d.as_secs_f64(),
        bytes
    );
    if who == Who::Svn {
        row.svn.push(d);
        row.svn_bytes.push(bytes);
    } else {
        row.engine.push(d);
        row.engine_bytes.push(bytes);
    }
}

fn run(a: &Args) -> Result<()> {
    let svn = Svn::find()?.ok_or_else(|| err("svn CLI not found"))?;
    if !Apache::available() {
        return Err(err("apache2 + mod_dav_svn not found"));
    }
    let apache = Apache::start()?;
    let upstream: u16 = apache
        .url("x")
        .rsplit_once(':')
        .and_then(|(_, r)| r.split('/').next())
        .and_then(|p| p.parse().ok())
        .ok_or_else(|| err("cannot read the server port"))?;
    let proxy = CountingProxy::start(upstream)?;
    let scratch = tempfile::Builder::new()
        .prefix("opensvn-commit-bench-")
        .tempdir()
        .map_err(|e| err(format!("tempdir: {e}")))?;
    let b = Bench {
        svn,
        apache,
        proxy,
        exe: std::env::current_exe().map_err(|e| err(format!("current_exe: {e}")))?,
        scratch: scratch.path().to_path_buf(),
    };
    let mut add = Row::new(&format!("commit: add {} files", a.files));
    let mut modify = Row::new(&format!("commit: modify {} files", a.modify_files));
    let mut big = Row::new(&format!(
        "commit: 1 byte changed in a {} MiB file",
        a.big_mb
    ));
    for rep in 0..a.reps {
        let order = if rep % 2 == 0 {
            [Who::Svn, Who::Engine]
        } else {
            [Who::Engine, Who::Svn]
        };
        for who in order {
            b.add_many(a, who, &mut add, rep)?;
            b.modify_many(a, who, &mut modify, rep)?;
            b.big_one_byte(a, who, &mut big, rep)?;
        }
    }

    let rows = [add, modify, big];
    let mut md = String::from("# Benchmark: commit, OpenSVN engine vs svn CLI\n\n");
    md.push_str(&format!(
        "- svn CLI {}; server: {}, HTTP on 127.0.0.1\n- machine: {}\n- filesystem of the working copies: {}\n- build: {}, repetitions: {} (median, min in brackets)\n- bytes: what the client sent to the server during the commit (median)\n\n",
        {
            let (x, y, z) = b.svn.version();
            format!("{x}.{y}.{z}")
        },
        crate::machine::server(&b.apache.url("")),
        crate::machine::machine(),
        crate::machine::filesystem(&b.scratch),
        if cfg!(debug_assertions) { "DEBUG (not representative)" } else { "release" },
        a.reps
    ));
    md.push_str("| operation | svn CLI | engine | engine / svn | svn bytes | engine bytes |\n|---|---:|---:|---:|---:|---:|\n");
    let min = |v: &[Duration]| v.iter().min().copied().unwrap_or_default();
    for r in &rows {
        md.push_str(&format!(
            "| {} | {:.2} s ({:.2}) | {:.2} s ({:.2}) | {:.2} | {} | {} |\n",
            r.name,
            median(&r.svn).as_secs_f64(),
            min(&r.svn).as_secs_f64(),
            median(&r.engine).as_secs_f64(),
            min(&r.engine).as_secs_f64(),
            r.ratio(),
            median(&r.svn_bytes),
            median(&r.engine_bytes)
        ));
    }
    if let Some(p) = a.out.parent() {
        std::fs::create_dir_all(p).map_err(|e| err(format!("mkdir {}: {e}", p.display())))?;
    }
    std::fs::write(&a.out, &md).map_err(|e| err(format!("write {}: {e}", a.out.display())))?;
    println!("{md}");
    Ok(())
}
