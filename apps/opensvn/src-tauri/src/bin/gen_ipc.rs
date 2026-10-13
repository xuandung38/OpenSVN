//! Writes `src/ipc/bindings.ts` from the Rust command/event registry (`pnpm gen:ipc`).
//! Needs no webview: it only walks the specta type graph.

use specta_typescript::Typescript;

fn main() {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "../src/ipc/bindings.ts".to_owned());
    opensvn_lib::builder()
        .export(Typescript::default(), &out)
        .unwrap_or_else(|e| panic!("failed to export bindings to {out}: {e}"));
    println!("wrote {out}");
}
