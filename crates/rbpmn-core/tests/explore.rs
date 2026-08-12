//! The invariant set (docs/stress-testing.md §1) applied exhaustively to the
//! fixture corpus and to generated models — §3 crossed with §7.
//!
//! The exploration machinery itself lives in `explorer`, shared with
//! `mutation.rs`.

mod explorer;
mod modelgen;

use explorer::assert_clean;
use modelgen::{Block, build};
use rbpmn_core::*;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

// ------------------------------------------------------------------ fixtures

#[derive(Deserialize)]
struct Scenario {
    fixture: String,
    #[serde(default)]
    bindings: Bindings,
    #[serde(default)]
    variables: Value,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../rbpmn-model/tests/fixtures")
}

/// Error codes the model declares — the alphabet for `RaiseError`.
fn declared_error_codes(xml: &str) -> Vec<String> {
    let mut codes = Vec::new();
    for part in xml.split("errorCode=\"").skip(1) {
        if let Some(end) = part.find('"') {
            let code = part[..end].to_string();
            if !codes.contains(&code) {
                codes.push(code);
            }
        }
    }
    codes
}

/// Every scenario's (fixture, bindings, variables) triple is a distinct
/// starting point worth exploring — including the ones that start an instance
/// into an immediate incident.
#[test]
fn corpus_state_spaces_hold_the_invariants() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("scenario directory")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no scenarios found in {}", dir.display());

    let mut seen = HashSet::new();
    let (mut explored, mut states) = (0usize, 0usize);
    for path in files {
        let sc: Scenario = serde_json::from_str(&fs::read_to_string(&path).unwrap())
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let key = format!(
            "{}|{}|{}",
            sc.fixture,
            serde_json::to_string(&sc.bindings).unwrap(),
            sc.variables
        );
        if !seen.insert(key) {
            continue;
        }

        let xml = fs::read_to_string(fixtures_dir().join(&sc.fixture)).unwrap();
        let defs = rbpmn_model::parse(&xml).unwrap();
        let proc = ExecutableProcess::compile(&defs, "p", &sc.bindings)
            .unwrap_or_else(|e| panic!("{}: {e}", sc.fixture));
        states += assert_clean(
            &sc.fixture,
            &proc,
            sc.variables.clone(),
            &declared_error_codes(&xml),
        );
        explored += 1;
    }
    assert!(explored > 0, "explored nothing");
    println!("corpus: {explored} starting points, {states} reachable states, all clean");
}

// ----------------------------------------------------------------- synthetic

/// Generated block-structured models (see `modelgen`), explored exhaustively:
/// docs/stress-testing.md §3 crossed with §7. The fixtures top out at three
/// parallel branches and never nest a loop inside one, so this is where the
/// interesting state spaces come from.
fn explore_block(label: &str, block: &Block) -> usize {
    let g = build(block);
    let defs = rbpmn_model::parse(&g.xml).expect("generated model parses");
    let proc = ExecutableProcess::compile(&defs, "p", &Bindings::default())
        .expect("generated model is block-structured and must compile");
    assert_clean(label, &proc, json!({}), &[])
}

/// `Par(branches x depth)` — the concurrency-scaling shape, now expressed in
/// the generator's grammar rather than hand-rolled XML.
fn par(branches: usize, depth: usize) -> Block {
    Block::Par(
        (0..branches)
            .map(|_| Block::Seq((0..depth).map(|_| Block::Task).collect()))
            .collect(),
    )
}

/// Shapes beyond what the fixtures reach, kept small enough that the whole
/// test stays in the tens of milliseconds. Cost is exponential in branch
/// width and only polynomial in depth, so depth is the cheap axis.
#[test]
fn generated_models_hold_the_invariants() {
    let shapes: Vec<(String, Block)> = vec![
        ("par(2x1)".into(), par(2, 1)),
        ("par(3x1)".into(), par(3, 1)),
        ("par(4x1)".into(), par(4, 1)),
        ("par(2x3)".into(), par(2, 3)),
        ("par(3x2)".into(), par(3, 2)),
        ("par(4x2)".into(), par(4, 2)),
        (
            "xor inside par".into(),
            Block::Par(vec![
                Block::Xor(vec![Block::Task, Block::Task]),
                Block::Task,
            ]),
        ),
        (
            "par inside xor".into(),
            Block::Xor(vec![
                Block::Par(vec![Block::Task, Block::Task]),
                Block::Task,
            ]),
        ),
        (
            "loop around a parallel block".into(),
            Block::Loop(Box::new(Block::Par(vec![Block::Task, Block::Task]))),
        ),
        (
            "loop inside a parallel branch".into(),
            Block::Par(vec![Block::Loop(Box::new(Block::Task)), Block::Task]),
        ),
        (
            "nested loops".into(),
            Block::Loop(Box::new(Block::Seq(vec![
                Block::Loop(Box::new(Block::Task)),
                Block::Task,
            ]))),
        ),
    ];

    let mut total = 0;
    for (label, block) in shapes {
        total += explore_block(&label, &block);
    }
    println!("generated: {total} reachable states, all clean");
}

/// Wider and deeper — `cargo test -- --ignored`. Cost is exponential in branch
/// width: past ~10 branches this runs for seconds, past ~16 for minutes.
#[test]
#[ignore = "wider sweep: run explicitly with --ignored"]
fn generated_models_hold_the_invariants_deeply() {
    let shapes: Vec<(String, Block)> = vec![
        ("par(5x2)".into(), par(5, 2)),
        ("par(6x2)".into(), par(6, 2)),
        ("par(4x4)".into(), par(4, 4)),
        ("par(2x20)".into(), par(2, 20)),
        (
            "par of loops".into(),
            Block::Par(vec![
                Block::Loop(Box::new(Block::Task)),
                Block::Loop(Box::new(Block::Task)),
                Block::Task,
            ]),
        ),
        (
            "loop around xor inside par".into(),
            Block::Loop(Box::new(Block::Par(vec![
                Block::Xor(vec![Block::Task, Block::Task]),
                Block::Seq(vec![Block::Task, Block::Task]),
            ]))),
        ),
    ];
    for (label, block) in shapes {
        let states = explore_block(&label, &block);
        println!("{label}: {states} states");
    }
}
