//! The `crash-worker` process: runs one engine operation and exits, or dies by `SIGKILL`.
//!
//! `crash-worker <state-dir> <op> <url> <wc> [message]`, op one of `checkout`, `update`,
//! `commit`, `revert`, `cleanup`, `recover`, `status`. The last stdout line is
//! `RESULT ok <details>` or `RESULT err <message>` (exit code 0 / 2).
//!
//! With feature `failpoints`: [`FP_ENV`] names a failpoint and [`FP_HIT_ENV`] the hit at which
//! the worker sends itself `SIGKILL` (no destructors, no flushing: like a crash or power cut of
//! the process). Hit `0` only counts: the worker prints `FP_HITS <n>` before `RESULT`.

use std::path::PathBuf;

use crate::engine::Engine;

/// Environment variable naming the failpoint to arm.
pub const FP_ENV: &str = "OPENSVN_CRASH_FP";
/// Environment variable with the 1-based hit that kills the worker (`0`: count only).
pub const FP_HIT_ENV: &str = "OPENSVN_CRASH_FP_HIT";

/// Sends `SIGKILL` to this process and never returns.
pub fn sigkill_self() -> ! {
    let pid = std::process::id().to_string();
    let _ = std::process::Command::new("kill")
        .args(["-KILL", &pid])
        .status();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

#[cfg(feature = "failpoints")]
mod arm {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Arms the failpoint from the environment; returns its hit counter.
    pub fn arm() -> Result<Option<Arc<AtomicU64>>, String> {
        let Ok(name) = std::env::var(super::FP_ENV) else {
            return Ok(None);
        };
        let kill_at: u64 = std::env::var(super::FP_HIT_ENV)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);
        let hits = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&hits);
        let fp = name.clone();
        fail::cfg_callback(name, move || {
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            if kill_at != 0 && n == kill_at {
                eprintln!("FP_KILL {fp} {n}");
                super::sigkill_self();
            }
        })?;
        Ok(Some(hits))
    }
}

#[cfg(not(feature = "failpoints"))]
mod arm {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    pub fn arm() -> Result<Option<Arc<AtomicU64>>, String> {
        if std::env::var_os(super::FP_ENV).is_some() {
            return Err("built without the `failpoints` feature".to_owned());
        }
        Ok(None)
    }
}

async fn run(
    engine: &Engine,
    op: &str,
    url: &str,
    wc: &std::path::Path,
    msg: &str,
) -> crate::Result<String> {
    Ok(match op {
        "checkout" => format!(
            "rev={:?}",
            engine.checkout(url, wc, None).await?.map(|r| r.0)
        ),
        "update" => format!("rev={:?}", engine.update(url, wc, None).await?.map(|r| r.0)),
        "commit" => format!("rev={:?}", engine.commit(url, wc, msg).await?.map(|r| r.0)),
        "revert" => format!("reverted={}", engine.revert(wc)?),
        "cleanup" => format!("work_items={}", engine.cleanup(wc)?),
        "status" => format!("entries={}", engine.status(wc).await?.len()),
        "recover" => {
            let r = engine.recover(url, wc).await?;
            let mut s = format!(
                "locks_removed={} locks_need_user={} work_items={} finalized={} aborted={} unsettled={}",
                r.locks_removed,
                r.locks_need_user,
                r.work_items,
                r.commits_finalized,
                r.commits_aborted,
                r.commits_unsettled.len()
            );
            for u in &r.commits_unsettled {
                s.push_str(&format!(" [{}]", u.replace('\n', " ")));
            }
            s
        }
        other => return Err(crate::err(format!("unknown op {other}"))),
    })
}

/// Entry point of the worker binary; returns the process exit code.
pub fn main() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 4 {
        eprintln!("usage: crash-worker <state-dir> <op> <url> <wc> [message]");
        return 64;
    }
    let hits = match arm::arm() {
        Ok(h) => h,
        Err(e) => {
            println!("RESULT err failpoint: {e}");
            return 3;
        }
    };
    let state = PathBuf::from(&args[0]);
    let (op, url, wc) = (&args[1], &args[2], PathBuf::from(&args[3]));
    let msg = args
        .get(4)
        .cloned()
        .unwrap_or_else(|| "crash test".to_owned());
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            println!("RESULT err runtime: {e}");
            return 3;
        }
    };
    let result = rt.block_on(async {
        let engine = Engine::new(&state)?;
        run(&engine, op, url, &wc, &msg).await
    });
    if let Some(h) = hits {
        println!("FP_HITS {}", h.load(std::sync::atomic::Ordering::SeqCst));
    }
    match result {
        Ok(detail) => {
            println!("RESULT ok {detail}");
            0
        }
        Err(e) => {
            println!("RESULT err {}", e.0.replace('\n', " "));
            2
        }
    }
}
