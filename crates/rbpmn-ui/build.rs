//! Fails early and legibly when the UI bundles have not been built yet.
//!
//! The bundles under `assets/` are compile output, not source: minified,
//! reproducible from their inputs, and edited by nobody. So they are
//! gitignored like every other generated artifact in this repo
//! (`playground/src/wasm/`, `bpmnlint-plugin-rbpmn/wasm/`,
//! `playground/src/fixtures.generated.js`) and produced by `just ui`.
//!
//! `include_str!` would fail on its own, but it would fail pointing at a line
//! in lib.rs, which tells nobody what to do about it. Hence this.

use std::path::Path;

const ASSETS: &[&str] = &["inspector.js", "inspector.css", "editor.js", "editor.css"];

fn main() {
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
    println!("cargo:rerun-if-changed=assets");

    let missing: Vec<&str> = ASSETS
        .iter()
        .copied()
        .filter(|file| !assets.join(file).is_file())
        .collect();
    if missing.is_empty() {
        return;
    }

    panic!(
        "the UI bundles have not been built: {} missing from {}.\n\n\
         Run `just ui` once (needs node and wasm-pack, the same tools \
         `just playground` and `just parity` already require). They are build \
         output, so they are not in git.",
        missing.join(", "),
        assets.display()
    );
}
