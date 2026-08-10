//! Dumps lint output for the whole fixture corpus as JSON, using the same
//! serialization as the WASM boundary (`rbpmn_wasm::lint_json`). The parity
//! check compares this byte-for-byte with what the playground's WASM build
//! produces — the guarantee that the playground never lies.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../rbpmn-model/tests/fixtures");
    let mut out = BTreeMap::new();
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
            out.insert(name, rbpmn_wasm::lint_json(&xml));
        }
    }
    println!("{}", serde_json::to_string(&out).expect("serializes"));
}
