//! The benchmark driver and the in-process engine side (`compare-svn engine ...`).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use opensvn_crash::engine::Engine;
use opensvn_crash::rng::Rng;
use opensvn_crash::server::Apache;
use opensvn_crash::svn::Svn;
use opensvn_crash::{Result, err};
use svn_types::Revnum;

struct Args {
    files: usize,
    binaries: usize,
    binary_mb: usize,
    update_files: usize,
    commit_files: usize,
    reps: usize,
    out: PathBuf,
}

fn parse(argv: &[String]) -> Result<Args> {
    let mut a = Args {
        files: 100_000,
        binaries: 4,
        binary_mb: 64,
        update_files: 10_000,
        commit_files: 1_000,
        reps: 3,
        out: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/bench-report/bench.md"),
    };
    let mut it = argv.iter();
    while let Some(k) = it.next() {
        let v = it.next().ok_or_else(|| err(format!("{k} needs a value")))?;
        let n = || v.parse::<usize>().map_err(|e| err(format!("{k} {v}: {e}")));
        match k.as_str() {
            "--files" => a.files = n()?,
            "--binaries" => a.binaries = n()?,
            "--binary-mb" => a.binary_mb = n()?,
            "--update-files" => a.update_files = n()?,
            "--commit-files" => a.commit_files = n()?,
            "--reps" => a.reps = n()?.max(1),
            "--out" => a.out = PathBuf::from(v),
            other => return Err(err(format!("unknown option {other}"))),
        }
    }
    a.update_files = a.update_files.min(a.files);
    // Every repetition of each client commits its own files (no out-of-date conflicts).
    a.commit_files = a.commit_files.min(a.files / (2 * a.reps));
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
            eprintln!("compare-svn: {e}");
            1
        }
    }
}

/// `compare-svn engine <state> checkout|update <url> <wc> [rev]` / `... status <wc>` /
/// `... commit <url> <wc> <message>`.
fn engine_cmd(args: &[String]) -> Result<()> {
    let get = |i: usize| {
        args.get(i)
            .map(String::as_str)
            .ok_or_else(|| err("usage: compare-svn engine <state> <op> ..."))
    };
    let state = PathBuf::from(get(0)?);
    let op = get(1)?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| err(format!("runtime: {e}")))?;
    rt.block_on(async {
        let engine = Engine::new(&state)?;
        match op {
            "checkout" | "update" => {
                let (url, wc) = (get(2)?, PathBuf::from(get(3)?));
                let rev = match args.get(4) {
                    Some(r) => Some(Revnum(
                        r.parse().map_err(|e| err(format!("revision {r}: {e}")))?,
                    )),
                    None => None,
                };
                let got = if op == "checkout" {
                    engine.checkout(url, &wc, rev).await?
                } else {
                    engine.update(url, &wc, rev).await?
                };
                println!("rev={:?}", got.map(|r| r.0));
            }
            "status" => {
                let n = engine.status(Path::new(get(2)?)).await?.len();
                println!("entries={n}");
            }
            "commit" => {
                let (url, wc, msg) = (get(2)?, PathBuf::from(get(3)?), get(4)?);
                let rev = engine.commit(url, &wc, msg).await?;
                println!("rev={:?}", rev.map(|r| r.0));
            }
            other => return Err(err(format!("unknown engine op {other}"))),
        }
        Ok(())
    })
}

/// Runs `cmd` to completion and returns its wall time; fails when it fails.
fn time(cmd: &mut Command, what: &str) -> Result<Duration> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let start = Instant::now();
    let out = cmd
        .output()
        .map_err(|e| err(format!("{what}: cannot run: {e}")))?;
    let el = start.elapsed();
    if !out.status.success() {
        return Err(err(format!(
            "{what}: {}{}",
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        )));
    }
    Ok(el)
}

/// Drops the page cache (as root, or through passwordless `sudo` as on CI runners). True when
/// it worked; containers usually refuse it.
fn drop_caches() -> bool {
    let _ = Command::new("sync").status();
    std::fs::write("/proc/sys/vm/drop_caches", "3\n").is_ok()
        || Command::new("sudo")
            .args(["-n", "sh", "-c", "echo 3 > /proc/sys/vm/drop_caches"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
}

fn median(v: &[Duration]) -> Duration {
    let mut s = v.to_vec();
    s.sort();
    s.get(s.len() / 2).copied().unwrap_or_default()
}

fn secs(d: Duration) -> String {
    format!("{:.2} s", d.as_secs_f64())
}

struct Row {
    name: String,
    svn: Vec<Duration>,
    engine: Vec<Duration>,
}

impl Row {
    fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            svn: Vec::new(),
            engine: Vec::new(),
        }
    }

    fn ratio(&self) -> f64 {
        median(&self.engine).as_secs_f64() / median(&self.svn).as_secs_f64().max(1e-9)
    }
}

struct Bench<'a> {
    svn: &'a Svn,
    exe: PathBuf,
    state: PathBuf,
}

impl Bench<'_> {
    fn engine(&self, args: &[&str]) -> Command {
        let mut c = Command::new(&self.exe);
        c.arg("engine").arg(&self.state).args(args);
        crate::machine::wrap(c)
    }

    fn svn(&self, args: &[&str]) -> Command {
        let mut c = self.svn.cmd();
        c.args(args);
        crate::machine::wrap(c)
    }

    /// The working copy is clean and at `rev` according to the svn CLI.
    fn check_clean(&self, wc: &Path, rev: u64, what: &str) -> Result<()> {
        let st = self.svn.run_in(wc, &["status"])?;
        if !st.trim().is_empty() {
            let head: Vec<&str> = st.lines().take(10).collect();
            return Err(err(format!(
                "{what}: svn status not clean:\n{}",
                head.join("\n")
            )));
        }
        let got = self.svn.run_in(wc, &["info", "--show-item", "revision"])?;
        if got.trim() != rev.to_string() {
            return Err(err(format!("{what}: at r{} instead of r{rev}", got.trim())));
        }
        Ok(())
    }
}

/// Seed of the generated repository (the same data on every run).
const SEED: u64 = 0x05EE_D0F0_BE5A;

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
    format!("t{:03}/s{}/f{i:06}.txt", i / 1000, (i / 100) % 10)
}

fn write(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p).map_err(|e| err(format!("mkdir {}: {e}", p.display())))?;
    }
    std::fs::write(path, bytes).map_err(|e| err(format!("write {}: {e}", path.display())))
}

/// Builds repository `big`: r1 = the files and binaries, r2 = `update_files` text files and the
/// first binary changed. Returns the total bytes of r1.
fn generate(a: &Args, svn: &Svn, apache: &Apache, scratch: &Path) -> Result<u64> {
    let mut rng = Rng::new(SEED);
    let repo = apache.repos_dir().join("big");
    svn.create_repo(&repo)?;
    let file_url = format!("file://{}", repo.display());
    let src = scratch.join("src");
    let mut total = 0u64;
    for i in 0..a.files {
        let t = text_file(&mut rng, i);
        total += t.len() as u64;
        write(&src.join(file_path(i)), &t)?;
    }
    for b in 0..a.binaries {
        let bytes = rng.bytes(a.binary_mb << 20);
        total += bytes.len() as u64;
        write(&src.join(format!("bin/b{b}.bin")), &bytes)?;
    }
    eprintln!(
        "importing {} files ({} MiB)...",
        a.files + a.binaries,
        total >> 20
    );
    svn.run(&[
        "import",
        "-q",
        "-m",
        "r1",
        &src.to_string_lossy(),
        &file_url,
    ])?;
    std::fs::remove_dir_all(&src).map_err(|e| err(format!("rm src: {e}")))?;

    // r2 through svnmucc: `put SRC URL` per changed file.
    let r2 = scratch.join("r2");
    let mut actions = String::new();
    let step = (a.files / a.update_files.max(1)).max(1);
    let mut changed = 0;
    for i in (0..a.files).step_by(step).take(a.update_files) {
        let local = r2.join(format!("{i}.txt"));
        let mut t = text_file(&mut Rng::new(SEED ^ i as u64), i);
        t.extend_from_slice(format!("changed in r2: {}\n", rng.next_u64()).as_bytes());
        write(&local, &t)?;
        actions.push_str(&format!("put\n{}\n{}\n", local.display(), file_path(i)));
        changed += 1;
    }
    if a.binaries > 0 {
        let local = r2.join("b0.bin");
        let mut bytes = Rng::new(1).bytes(a.binary_mb << 20);
        let mid = bytes.len() / 2;
        let patch = rng.bytes(1 << 20);
        let end = (mid + patch.len()).min(bytes.len());
        bytes[mid..end].copy_from_slice(&patch[..end - mid]);
        write(&local, &bytes)?;
        actions.push_str(&format!("put\n{}\nbin/b0.bin\n", local.display()));
    }
    let args_file = scratch.join("r2-actions");
    write(&args_file, actions.as_bytes())?;
    let out = Command::new("svnmucc")
        .args(["--non-interactive", "--config-dir"])
        .arg(scratch.join("svnmucc-config"))
        .args(["-m", "r2", "-U", &file_url, "-X"])
        .arg(&args_file)
        .env("LC_ALL", "C.UTF-8")
        .output()
        .map_err(|e| err(format!("svnmucc: {e}")))?;
    if !out.status.success() {
        return Err(err(format!(
            "svnmucc: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    std::fs::remove_dir_all(&r2).map_err(|e| err(format!("rm r2: {e}")))?;
    eprintln!("r2 changes {changed} text files and one binary");
    apache.hand_over("big")?;
    Ok(total)
}

fn version_of(cmd: &str, arg: &str) -> String {
    Command::new(cmd)
        .arg(arg)
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned()
        })
        .unwrap_or_else(|| "unknown".into())
}

fn run(a: &Args) -> Result<()> {
    let svn = Svn::find()?.ok_or_else(|| err("svn CLI not found"))?;
    if !Apache::available() {
        return Err(err("apache2 + mod_dav_svn not found"));
    }
    let apache = Apache::start()?;
    let scratch = tempfile::Builder::new()
        .prefix("opensvn-bench-")
        .tempdir()
        .map_err(|e| err(format!("tempdir: {e}")))?;
    let total = generate(a, &svn, &apache, scratch.path())?;
    let url = apache.url("big");
    let exe = std::env::current_exe().map_err(|e| err(format!("current_exe: {e}")))?;
    let b = Bench {
        svn: &svn,
        exe,
        state: scratch.path().join("engine-state"),
    };
    let wc_svn = scratch.path().join("wc-svn");
    let wc_eng = scratch.path().join("wc-engine");
    let (svn_wc, eng_wc) = (wc_svn.to_string_lossy(), wc_eng.to_string_lossy());

    let mut checkout = Row::new("checkout HEAD");
    for rep in 0..a.reps {
        for dir in [&wc_svn, &wc_eng] {
            let _ = std::fs::remove_dir_all(dir);
        }
        // Alternate who goes first so neither always gets the warmer server cache.
        let order = if rep % 2 == 0 { [0, 1] } else { [1, 0] };
        for who in order {
            if who == 0 {
                let d = time(
                    &mut b.svn(&["checkout", "-q", &url, &svn_wc]),
                    "svn checkout",
                )?;
                checkout.svn.push(d);
            } else {
                let d = time(
                    &mut b.engine(&["checkout", &url, &eng_wc]),
                    "engine checkout",
                )?;
                checkout.engine.push(d);
            }
        }
        eprintln!(
            "checkout rep {rep}: svn {} engine {}",
            secs(*checkout.svn.last().unwrap_or(&Duration::ZERO)),
            secs(*checkout.engine.last().unwrap_or(&Duration::ZERO))
        );
    }
    b.check_clean(&wc_eng, 2, "engine checkout")?;

    // Status on the svn CLI's working copy (same input for both tools).
    let cold = drop_caches();
    let mut status_first = Row::new(if cold {
        "status, first run (page cache dropped)"
    } else {
        "status, first run after checkout (page cache warm)"
    });
    let d = time(&mut b.svn(&["status", &svn_wc]), "svn status")?;
    status_first.svn.push(d);
    drop_caches();
    let d = time(&mut b.engine(&["status", &svn_wc]), "engine status")?;
    status_first.engine.push(d);
    let mut status_repeat = Row::new("status, repeat run");
    for _ in 0..a.reps.max(3) {
        status_repeat
            .svn
            .push(time(&mut b.svn(&["status", &svn_wc]), "svn status")?);
        status_repeat
            .engine
            .push(time(&mut b.engine(&["status", &svn_wc]), "engine status")?);
    }
    let mut status_own = Row::new("status, repeat run, each tool on its own checkout");
    for _ in 0..a.reps.max(3) {
        status_own
            .svn
            .push(time(&mut b.svn(&["status", &svn_wc]), "svn status")?);
        status_own
            .engine
            .push(time(&mut b.engine(&["status", &eng_wc]), "engine status")?);
    }

    // Update r1 -> r2 on each tool's own working copy.
    let mut update = Row::new(&format!(
        "update r1 -> r2 ({} files + 1 binary)",
        a.update_files
    ));
    for rep in 0..a.reps {
        time(
            &mut b.svn(&["update", "-q", "-r", "1", &svn_wc]),
            "svn update -r1",
        )?;
        time(
            &mut b.engine(&["update", &url, &eng_wc, "1"]),
            "engine update -r1",
        )?;
        let order = if rep % 2 == 0 { [0, 1] } else { [1, 0] };
        for who in order {
            if who == 0 {
                update.svn.push(time(
                    &mut b.svn(&["update", "-q", "-r", "2", &svn_wc]),
                    "svn update",
                )?);
            } else {
                update.engine.push(time(
                    &mut b.engine(&["update", &url, &eng_wc, "2"]),
                    "engine update",
                )?);
            }
        }
        b.check_clean(&wc_eng, 2, "engine update")?;
    }

    // Commit of `commit_files` modified text files, each client from its own working copy and
    // on its own files.
    let mut commit = Row::new(&format!("commit ({} modified files)", a.commit_files));
    for rep in 0..a.reps {
        let order = if rep % 2 == 0 { [0, 1] } else { [1, 0] };
        for who in order {
            let (wc, k) = if who == 0 {
                (&wc_svn, 2 * rep)
            } else {
                (&wc_eng, 2 * rep + 1)
            };
            let mut last = (String::new(), Vec::new());
            for j in 0..a.commit_files {
                let i = k + j * 2 * a.reps;
                let p = wc.join(file_path(i));
                let mut t = std::fs::read(&p).map_err(|e| err(format!("read: {e}")))?;
                t.extend_from_slice(format!("committed in rep {rep}\n").as_bytes());
                write(&p, &t)?;
                last = (file_path(i), t);
            }
            let msg = format!("commit rep {rep}");
            let w = wc.to_string_lossy().into_owned();
            if who == 0 {
                commit.svn.push(time(
                    &mut b.svn(&["commit", "-q", "-m", &msg, &w]),
                    "svn commit",
                )?);
            } else {
                commit.engine.push(time(
                    &mut b.engine(&["commit", &url, &w, &msg]),
                    "engine commit",
                )?);
            }
            let st = svn.run_in(wc, &["status", "-q"])?;
            if !st.trim().is_empty() {
                return Err(err(format!("{msg}: not clean after the commit:\n{st}")));
            }
            if svn.cat(&format!("{url}/{}", last.0))? != last.1 {
                return Err(err(format!("{msg}: {} has the wrong content", last.0)));
            }
        }
        eprintln!(
            "commit rep {rep}: svn {} engine {}",
            secs(*commit.svn.last().unwrap_or(&Duration::ZERO)),
            secs(*commit.engine.last().unwrap_or(&Duration::ZERO))
        );
    }

    let rows = [
        checkout,
        update,
        status_first,
        status_repeat,
        status_own,
        commit,
    ];
    let repeat_engine = median(&rows[3].engine);
    let mut md = String::from("# Benchmark: OpenSVN engine vs svn CLI\n\n");
    md.push_str(&format!(
        "- svn CLI: {} ({})\n- server: {}, HTTP on 127.0.0.1\n- dataset: {} text files + {} x {} MiB binaries, {} MiB in r1\n- machine: {}\n- filesystem of the working copies: {}\n- build: {}\n- repetitions: {} (median shown, min in brackets; every repetition below)\n\n",
        version_of(&svn.cmd().get_program().to_string_lossy(), "--version"),
        {
            let (x, y, z) = svn.version();
            format!("{x}.{y}.{z}")
        },
        crate::machine::server(&url),
        a.files,
        a.binaries,
        a.binary_mb,
        total >> 20,
        crate::machine::machine(),
        crate::machine::filesystem(scratch.path()),
        if cfg!(debug_assertions) { "DEBUG (not representative)" } else { "release" },
        a.reps
    ));
    md.push_str("| operation | svn CLI | engine | engine / svn |\n|---|---:|---:|---:|\n");
    for r in &rows {
        let min = |v: &[Duration]| v.iter().min().copied().unwrap_or_default();
        md.push_str(&format!(
            "| {} | {} ({}) | {} ({}) | {:.2} |\n",
            r.name,
            secs(median(&r.svn)),
            secs(min(&r.svn)),
            secs(median(&r.engine)),
            secs(min(&r.engine)),
            r.ratio()
        ));
    }
    let slower: Vec<&str> = rows
        .iter()
        .filter(|r| r.ratio() > 1.0)
        .map(|r| r.name.as_str())
        .collect();
    md.push_str(&format!(
        "\n- Not slower than svn CLI: {}\n- Repeat status < 3 s on {} files: {} ({})\n",
        if slower.is_empty() {
            "holds for every operation".to_owned()
        } else {
            format!("**does not hold** for: {}", slower.join("; "))
        },
        a.files,
        if repeat_engine < Duration::from_secs(3) {
            "holds"
        } else {
            "**does not hold**"
        },
        secs(repeat_engine)
    ));
    md.push_str("\n## Every repetition\n\n| operation | svn CLI | engine |\n|---|---|---|\n");
    let all = |v: &[Duration]| {
        v.iter()
            .map(|d| format!("{:.2}", d.as_secs_f64()))
            .collect::<Vec<_>>()
            .join(", ")
    };
    for r in &rows {
        md.push_str(&format!(
            "| {} | {} | {} |\n",
            r.name,
            all(&r.svn),
            all(&r.engine)
        ));
    }
    md.push_str("\nEach number is the wall time of a separate process (`svn` or `compare-svn engine`), process start included. Every engine checkout/update is followed by an untimed `svn status` + `svn info` check (clean, right revision); every commit by `svn status` (clean) and `svn cat` of a committed file.\n");
    if let Some(p) = a.out.parent() {
        std::fs::create_dir_all(p).map_err(|e| err(format!("mkdir {}: {e}", p.display())))?;
    }
    std::fs::write(&a.out, &md).map_err(|e| err(format!("write {}: {e}", a.out.display())))?;
    let mut json = String::from("[");
    for (i, r) in rows.iter().enumerate() {
        json.push_str(&format!(
            "{}{{\"op\":{:?},\"svn_s\":{:.3},\"engine_s\":{:.3},\"ratio\":{:.3}}}",
            if i > 0 { "," } else { "" },
            r.name,
            median(&r.svn).as_secs_f64(),
            median(&r.engine).as_secs_f64(),
            r.ratio()
        ));
    }
    json.push(']');
    std::fs::write(a.out.with_extension("json"), json)
        .map_err(|e| err(format!("write json: {e}")))?;
    println!("{md}");
    eprintln!("report: {}", a.out.display());
    Ok(())
}
