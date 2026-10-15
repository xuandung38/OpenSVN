//! The log benchmark driver and its in-process engine side
//! (`compare-svn-log engine <state> log <url> <path> <limit|-> <q|v>`).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use opensvn_crash::engine::Engine;
use opensvn_crash::server::Apache;
use opensvn_crash::svn::Svn;
use opensvn_crash::{Result, err};

struct Args {
    revisions: usize,
    files: usize,
    reps: usize,
    out: PathBuf,
}

fn parse(argv: &[String]) -> Result<Args> {
    let mut a = Args {
        revisions: 100_000,
        files: 100,
        reps: 3,
        out: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target/bench-report/log.md"),
    };
    let mut it = argv.iter();
    while let Some(k) = it.next() {
        let v = it.next().ok_or_else(|| err(format!("{k} needs a value")))?;
        let n = || v.parse::<usize>().map_err(|e| err(format!("{k} {v}: {e}")));
        match k.as_str() {
            "--revisions" => a.revisions = n()?.max(2),
            "--files" => a.files = n()?.max(1),
            "--reps" => a.reps = n()?.max(1),
            "--out" => a.out = PathBuf::from(v),
            other => return Err(err(format!("unknown option {other}"))),
        }
    }
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
            eprintln!("compare-svn-log: {e}");
            1
        }
    }
}

/// `compare-svn-log engine <state> log <url> <path> <limit|-> <q|v>`: prints
/// `entries=<n> changed=<n>`.
fn engine_cmd(args: &[String]) -> Result<()> {
    let get = |i: usize| {
        args.get(i).map(String::as_str).ok_or_else(|| {
            err("usage: compare-svn-log engine <state> log <url> <path> <limit|-> <q|v>")
        })
    };
    let state = PathBuf::from(get(0)?);
    if get(1)? != "log" {
        return Err(err(format!("unknown engine op {}", get(1)?)));
    }
    let (url, path) = (get(2)?, get(3)?);
    let limit = match get(4)? {
        "-" => None,
        l => Some(l.parse().map_err(|e| err(format!("limit {l}: {e}")))?),
    };
    let verbose = get(5)? == "v";
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| err(format!("runtime: {e}")))?;
    rt.block_on(async {
        let engine = Engine::new(&state)?;
        let (entries, changed) = engine.log(url, path, limit, verbose).await?;
        println!("entries={entries} changed={changed}");
        Ok(())
    })
}

/// Runs `cmd`, returns its wall time and stdout; fails when it fails.
fn time(cmd: &mut Command, what: &str) -> Result<(Duration, String)> {
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
            "{what}: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok((el, String::from_utf8_lossy(&out.stdout).into_owned()))
}

fn median(v: &[Duration]) -> Duration {
    let mut s = v.to_vec();
    s.sort();
    s.get(s.len() / 2).copied().unwrap_or_default()
}

fn file_path(i: usize) -> String {
    format!("d{}/f{i:04}.txt", i % 10)
}

/// Writes one dump record: headers, then the property block and the text.
fn record(
    w: &mut impl std::io::Write,
    headers: &[String],
    props: Option<&str>,
    text: Option<&[u8]>,
) -> std::io::Result<()> {
    for h in headers {
        writeln!(w, "{h}")?;
    }
    let plen = props.map(str::len);
    let tlen = text.map(<[u8]>::len);
    if let Some(p) = plen {
        writeln!(w, "Prop-content-length: {p}")?;
    }
    if let Some(t) = tlen {
        writeln!(w, "Text-content-length: {t}")?;
    }
    if plen.is_some() || tlen.is_some() {
        writeln!(
            w,
            "Content-length: {}",
            plen.unwrap_or(0) + tlen.unwrap_or(0)
        )?;
    }
    writeln!(w)?;
    if let Some(p) = props {
        w.write_all(p.as_bytes())?;
    }
    if let Some(t) = text {
        w.write_all(t)?;
    }
    writeln!(w)?;
    Ok(())
}

fn prop(out: &mut String, k: &str, v: &str) {
    out.push_str(&format!("K {}\n{k}\nV {}\n{v}\n", k.len(), v.len()));
}

fn revprops(rev: usize) -> String {
    let mut p = String::new();
    prop(&mut p, "svn:author", "bench");
    // One second per revision from 2020-01-01 (well below 2^31 s for 100k revisions).
    let t = 1_577_836_800 + rev as i64;
    let (days, secs) = (t.div_euclid(86_400), t.rem_euclid(86_400));
    let (y, m, d) = civil(days);
    prop(
        &mut p,
        "svn:date",
        &format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.000000Z",
            secs / 3600,
            secs / 60 % 60,
            secs % 60
        ),
    );
    if rev > 0 {
        prop(&mut p, "svn:log", &format!("change {rev}: one file edited"));
    }
    p.push_str("PROPS-END\n");
    p
}

/// Days since 1970-01-01 to (year, month, day) (Howard Hinnant's `civil_from_days`).
fn civil(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// The dump of the benchmark repository.
fn write_dump(a: &Args, path: &Path) -> Result<()> {
    let f = std::fs::File::create(path).map_err(|e| err(format!("create dump: {e}")))?;
    let mut w = std::io::BufWriter::new(f);
    let io = |e: std::io::Error| err(format!("write dump: {e}"));
    writeln!(w, "SVN-fs-dump-format-version: 2\n").map_err(io)?;
    writeln!(w, "UUID: 0b0e5c4e-1a6e-4c5e-9d7b-00000000b10c\n").map_err(io)?;
    for rev in 0..=a.revisions {
        let props = revprops(rev);
        record(
            &mut w,
            &[format!("Revision-number: {rev}")],
            Some(&props),
            None,
        )
        .map_err(io)?;
        if rev == 0 {
            continue;
        }
        if rev == 1 {
            for d in 0..10.min(a.files) {
                record(
                    &mut w,
                    &[
                        format!("Node-path: d{d}"),
                        "Node-kind: dir".into(),
                        "Node-action: add".into(),
                    ],
                    Some("PROPS-END\n"),
                    None,
                )
                .map_err(io)?;
            }
            for i in 0..a.files {
                let text = format!("file {i}\n");
                record(
                    &mut w,
                    &[
                        format!("Node-path: {}", file_path(i)),
                        "Node-kind: file".into(),
                        "Node-action: add".into(),
                    ],
                    Some("PROPS-END\n"),
                    Some(text.as_bytes()),
                )
                .map_err(io)?;
            }
            continue;
        }
        let i = rev % a.files;
        let text = format!("file {i}\nchanged in r{rev}\n");
        record(
            &mut w,
            &[
                format!("Node-path: {}", file_path(i)),
                "Node-kind: file".into(),
                "Node-action: change".into(),
            ],
            None,
            Some(text.as_bytes()),
        )
        .map_err(io)?;
    }
    w.flush().map_err(io)
}

struct Row {
    name: String,
    svn: Vec<Duration>,
    engine: Vec<Duration>,
}

fn count_entries(svn_out: &str) -> usize {
    svn_out
        .lines()
        .filter(|l| {
            l.strip_prefix('r')
                .and_then(|r| r.split_once(" | "))
                .is_some_and(|(n, _)| n.chars().all(|c| c.is_ascii_digit()))
        })
        .count()
}

/// Changed-path lines of `svn log -v` (`   M /d0/f0000.txt`).
fn count_changed(svn_out: &str) -> usize {
    svn_out
        .lines()
        .filter(|l| {
            l.len() > 5
                && l.starts_with("   ")
                && matches!(l.as_bytes()[3], b'A' | b'M' | b'D' | b'R')
                && l.as_bytes()[4] == b' '
        })
        .count()
}

fn run(a: &Args) -> Result<()> {
    let svn = Svn::find()?.ok_or_else(|| err("svn CLI not found"))?;
    if !Apache::available() {
        return Err(err("apache2 + mod_dav_svn not found"));
    }
    let apache = Apache::start()?;
    let scratch = tempfile::Builder::new()
        .prefix("opensvn-log-bench-")
        .tempdir()
        .map_err(|e| err(format!("tempdir: {e}")))?;
    let repo = apache.repos_dir().join("log");
    svn.create_repo(&repo)?;
    let dump = scratch.path().join("log.dump");
    eprintln!("writing a dump of {} revisions...", a.revisions);
    write_dump(a, &dump)?;
    eprintln!("svnadmin load...");
    let t = Instant::now();
    svn.load(&repo, &dump)?;
    eprintln!("loaded in {:.1} s", t.elapsed().as_secs_f64());
    apache.hand_over("log")?;
    let url = apache.url("log");
    let exe = std::env::current_exe().map_err(|e| err(format!("current_exe: {e}")))?;
    let state = scratch.path().join("engine-state");
    let one = file_path(0);
    let one_url = format!("{url}/{one}");
    // (name, svn args, engine path, engine limit, verbose)
    let cases: Vec<(String, Vec<String>, String, String, bool)> = vec![
        (
            format!("log, all {} revisions (`-q`)", a.revisions),
            vec!["log".into(), "-q".into(), url.clone()],
            String::new(),
            "-".into(),
            false,
        ),
        (
            format!(
                "log, all {} revisions with changed paths (`-v`)",
                a.revisions
            ),
            vec!["log".into(), "-v".into(), url.clone()],
            String::new(),
            "-".into(),
            true,
        ),
        (
            "log, newest 100 (`-q -l 100`)".into(),
            vec![
                "log".into(),
                "-q".into(),
                "-l".into(),
                "100".into(),
                url.clone(),
            ],
            String::new(),
            "100".into(),
            false,
        ),
        (
            format!(
                "log of one file ({} revisions, `-q`)",
                a.revisions / a.files
            ),
            vec!["log".into(), "-q".into(), one_url],
            one.clone(),
            "-".into(),
            false,
        ),
    ];
    let mut rows = Vec::new();
    for (name, svn_args, path, limit, verbose) in &cases {
        let mut row = Row {
            name: name.clone(),
            svn: Vec::new(),
            engine: Vec::new(),
        };
        for rep in 0..a.reps {
            let order = if rep % 2 == 0 { [0, 1] } else { [1, 0] };
            for who in order {
                if who == 0 {
                    let mut c = svn.cmd();
                    c.args(svn_args);
                    let (d, out) = time(&mut c, "svn log")?;
                    row.svn.push(d);
                    let (n, ch) = (count_entries(&out), count_changed(&out));
                    // The engine must report exactly what svn printed.
                    let mut c = Command::new(&exe);
                    c.arg("engine")
                        .arg(&state)
                        .args(["log", url.as_str(), path.as_str(), limit.as_str()])
                        .arg(if *verbose { "v" } else { "q" });
                    let (_, eout) = time(&mut c, "engine log (check)")?;
                    let want = format!("entries={n} changed={}", if *verbose { ch } else { 0 });
                    if eout.trim() != want {
                        return Err(err(format!(
                            "{name}: engine reported `{}`, svn printed {want}",
                            eout.trim()
                        )));
                    }
                } else {
                    let mut c = Command::new(&exe);
                    c.arg("engine")
                        .arg(&state)
                        .args(["log", url.as_str(), path.as_str(), limit.as_str()])
                        .arg(if *verbose { "v" } else { "q" });
                    let (d, _) = time(&mut c, "engine log")?;
                    row.engine.push(d);
                }
            }
            eprintln!(
                "{name} rep {rep}: svn {:.2} s engine {:.2} s",
                row.svn.last().copied().unwrap_or_default().as_secs_f64(),
                row.engine.last().copied().unwrap_or_default().as_secs_f64()
            );
        }
        rows.push(row);
    }

    let (x, y, z) = svn.version();
    let mut md = String::from("# Benchmark: log, OpenSVN engine vs svn CLI\n\n");
    md.push_str(&format!(
        "- svn CLI {x}.{y}.{z}; server: {}, HTTP on 127.0.0.1\n- repository: {} revisions, each but r1 changing one of {} files\n- machine: {}\n- filesystem of the repository: {}\n- build: {}\n- repetitions: {} (median shown, min in brackets; every repetition below)\n\n",
        crate::machine::server(&url),
        a.revisions,
        a.files,
        crate::machine::machine(),
        crate::machine::filesystem(&repo),
        if cfg!(debug_assertions) { "DEBUG (not representative)" } else { "release" },
        a.reps
    ));
    md.push_str("| operation | svn CLI | engine | engine / svn |\n|---|---:|---:|---:|\n");
    let min = |v: &[Duration]| v.iter().min().copied().unwrap_or_default();
    for r in &rows {
        md.push_str(&format!(
            "| {} | {:.2} s ({:.2}) | {:.2} s ({:.2}) | {:.2} |\n",
            r.name,
            median(&r.svn).as_secs_f64(),
            min(&r.svn).as_secs_f64(),
            median(&r.engine).as_secs_f64(),
            min(&r.engine).as_secs_f64(),
            median(&r.engine).as_secs_f64() / median(&r.svn).as_secs_f64().max(1e-9)
        ));
    }
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
    md.push_str("\nEach number is the wall time of a separate process, process start included. `svn log` writes its output to a pipe the benchmark reads; the engine only counts entries. Before each timed engine run, an untimed engine run must report as many entries (and changed paths with `-v`) as svn printed.\n");
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
            median(&r.engine).as_secs_f64() / median(&r.svn).as_secs_f64().max(1e-9)
        ));
    }
    json.push(']');
    std::fs::write(a.out.with_extension("json"), json)
        .map_err(|e| err(format!("write json: {e}")))?;
    println!("{md}");
    Ok(())
}
