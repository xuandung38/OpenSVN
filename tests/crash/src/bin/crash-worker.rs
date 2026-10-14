//! One engine operation in its own process, for the crash suite to kill. See
//! `opensvn_crash::worker`.

#[cfg(target_os = "linux")]
fn main() {
    std::process::exit(opensvn_crash::worker::main());
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("crash-worker runs on Linux only");
    std::process::exit(64);
}
