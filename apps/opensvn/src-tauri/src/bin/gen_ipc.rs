//! Writes `src/ipc/bindings.ts` from the Rust command/event registry (`pnpm gen:ipc`).
//! Needs no webview: it only walks the specta type graph.

use specta_typescript::Typescript;

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "../src/ipc/bindings.ts".to_owned());
    if let Err(e) = opensvn_lib::builder::<tauri::Wry>().export(Typescript::default(), &out) {
        eprintln!("failed to export bindings to {out}: {e}");
        std::process::exit(1);
    }
    println!("wrote {out}");
}
