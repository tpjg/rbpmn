//! Dumps the fixture corpora through the WASM-boundary exports
//! (`rbpmn_wasm::lint_json`, `check_json` and, with the `dmn` feature,
//! `evaluate_json`), using the same serialization the browser sees. The parity
//! check compares this byte-for-byte with what the WASM build produces — the
//! guarantee that the playground never lies.
//!
//! `check_json` runs over the BPMN corpus, driving the compile stage (phase
//! gating, correlation bindings, topic resolution) that `lint_json` never
//! reaches. Each fixture is paired with its `.bindings.json` sidecar where
//! the corpus writes one, and an empty manifest otherwise: a `.bpmn` is half
//! a deployment, and comparing the halves separately would leave the manifest
//! rules (`decision-has-binding`, `ambiguous-message-arm`,
//! `config-binds-task`) uncompared between the two builds.
//!
//! It also runs over the **DMN corpus**, one artifact at a time against a
//! fixed minimal process. Decisions are where native and WASM are most likely
//! to drift, because that path runs dsntk — including a decimal
//! implementation this project substituted — so leaving it out of parity
//! would leave out the only part with a plausible reason to diverge.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// A minimal, valid process to pair every DMN artifact with, so `check_json`
/// reaches the decision half rather than short-circuiting on the model.
const HOST_PROCESS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL" id="defs">
  <bpmn:process id="p" isExecutable="true">
    <bpmn:startEvent id="start"><bpmn:outgoing>f1</bpmn:outgoing></bpmn:startEvent>
    <bpmn:endEvent id="end"><bpmn:incoming>f1</bpmn:incoming></bpmn:endEvent>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="end" />
  </bpmn:process>
</bpmn:definitions>"#;

fn fixtures(dir: &Path, extension: &str) -> Vec<(String, String)> {
    let mut paths: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|ext| ext == extension))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            (name, fs::read_to_string(&path).expect("fixture readable"))
        })
        .collect()
}

fn main() {
    let crates = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
    let bpmn_root = crates.join("rbpmn-model/tests/fixtures");

    let mut lint = BTreeMap::new();
    let mut check = BTreeMap::new();
    for dir in ["accept", "reject"] {
        for (file, xml) in fixtures(&bpmn_root.join(dir), "bpmn") {
            let name = format!("{dir}/{file}");
            let sidecar = bpmn_root
                .join(dir)
                .join(file.replace(".bpmn", ".bindings.json"));
            let bindings = fs::read_to_string(&sidecar)
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "{}".to_string());
            lint.insert(name.clone(), rbpmn_wasm::lint_json(&xml));
            check.insert(name, rbpmn_wasm::check_json(&xml, &bindings, "[]"));
        }
    }

    // The DMN corpus, each artifact bundled with the same host process.
    let mut decisions = BTreeMap::new();
    let dmn_root = crates.join("rbpmn-dmn/tests/fixtures");
    if dmn_root.is_dir() {
        for dir in ["accept", "reject"] {
            for (file, dmn) in fixtures(&dmn_root.join(dir), "dmn") {
                let bundle = serde_json::to_string(&vec![dmn]).expect("serializes");
                decisions.insert(
                    format!("{dir}/{file}"),
                    rbpmn_wasm::check_json(HOST_PROCESS, "{}", &bundle),
                );
            }
        }
    }

    let out = serde_json::json!({
        "lint": lint,
        "check": check,
        "decisions": decisions,
    });
    println!("{}", serde_json::to_string(&out).expect("serializes"));
}
