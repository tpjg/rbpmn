//! Dumps the whole fixture corpus through both WASM-boundary exports
//! (`rbpmn_wasm::lint_json` and `rbpmn_wasm::check_json`), using the same
//! serialization the browser sees. The parity check compares this
//! byte-for-byte with what the playground's WASM build produces — the
//! guarantee that the playground never lies.
//!
//! `check_json` runs with an empty manifest, which is the interesting case:
//! it drives the compile stage (phase gating, correlation bindings, topic
//! resolution) that `lint_json` never reaches, so parity covers it too.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../rbpmn-model/tests/fixtures");
    let mut lint = BTreeMap::new();
    let mut check = BTreeMap::new();
    for dir in ["accept", "reject"] {
        let mut paths: Vec<_> = fs::read_dir(root.join(dir))
            .expect("fixture dir")
            .map(|e| e.expect("dir entry").path())
            .filter(|p| p.extension().is_some_and(|ext| ext == "bpmn"))
            .collect();
        paths.sort();
        for path in paths {
            let name = format!("{dir}/{}", path.file_name().unwrap().to_string_lossy());
            let xml = fs::read_to_string(&path).expect("fixture readable");
            lint.insert(name.clone(), rbpmn_wasm::lint_json(&xml));
            check.insert(name, rbpmn_wasm::check_json(&xml, "{}"));
        }
    }
    let out = serde_json::json!({ "lint": lint, "check": check });
    println!("{}", serde_json::to_string(&out).expect("serializes"));
}
