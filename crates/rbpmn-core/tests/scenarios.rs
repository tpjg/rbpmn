//! Scenario corpus runner (design brief, testing strategy #1).
//!
//! Every `tests/scenarios/*.json` drives one fixture from the shared corpus:
//! initial variables, an ordered list of external interactions, and the
//! expected **golden event trace** plus final variable document and status.
//! Traces are compared line by line — the `Display` format of `Event` is
//! stable API, like rule IDs.

use rbpmn_core::{Bindings, Command, ExecutableProcess, InstanceState, InstanceStatus, step};
use serde::Deserialize;
use serde_json::Value;
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

#[derive(Deserialize)]
struct Scenario {
    fixture: String,
    #[serde(default)]
    bindings: Bindings,
    #[serde(default)]
    variables: Value,
    #[serde(default)]
    actions: Vec<Action>,
    expect: Expect,
}

#[derive(Deserialize)]
struct Action {
    complete: String,
    #[serde(default)]
    patch: Option<Value>,
}

#[derive(Deserialize)]
struct Expect {
    status: String,
    variables: Value,
    trace: Vec<String>,
}

fn run_scenario(path: &Path, failures: &mut String) {
    let name = path.file_name().unwrap().to_string_lossy();
    let scenario: Scenario = serde_json::from_str(&fs::read_to_string(path).unwrap())
        .unwrap_or_else(|e| {
            panic!("{name}: bad scenario JSON: {e}");
        });

    let fixture_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../rbpmn-model/tests/fixtures")
        .join(&scenario.fixture);
    let xml = fs::read_to_string(&fixture_path)
        .unwrap_or_else(|e| panic!("{name}: cannot read {}: {e}", fixture_path.display()));
    let defs = rbpmn_model::parse(&xml).unwrap();
    let process_id = defs.processes[0].id.clone();
    let proc = ExecutableProcess::compile(&defs, &process_id, &scenario.bindings)
        .unwrap_or_else(|e| panic!("{name}: compile failed: {e}"));

    let mut state = InstanceState::new();
    let mut trace: Vec<String> = Vec::new();

    let variables = if scenario.variables.is_null() {
        serde_json::json!({})
    } else {
        scenario.variables.clone()
    };
    let events = step(&proc, &mut state, Command::Start { variables }).expect("start");
    trace.extend(events.iter().map(|e| e.to_string()));

    for action in &scenario.actions {
        let node = proc
            .node_by_id(&action.complete)
            .unwrap_or_else(|| panic!("{name}: no element '{}'", action.complete));
        let id = state
            .open_work_item_at(node)
            .unwrap_or_else(|| panic!("{name}: no open work item at '{}'", action.complete));
        let patch = action
            .patch
            .clone()
            .unwrap_or_else(|| serde_json::json!({}));
        let events = step(&proc, &mut state, Command::CompleteWorkItem { id, patch })
            .unwrap_or_else(|e| panic!("{name}: complete '{}' failed: {e}", action.complete));
        trace.extend(events.iter().map(|e| e.to_string()));
    }

    let status = match state.status {
        InstanceStatus::Created => "created",
        InstanceStatus::Active => "active",
        InstanceStatus::Completed => "completed",
        InstanceStatus::Terminated => "terminated",
    };

    let mut problems = Vec::new();
    if status != scenario.expect.status {
        problems.push(format!(
            "status: expected {}, got {status}",
            scenario.expect.status
        ));
    }
    if state.variables != scenario.expect.variables {
        problems.push(format!(
            "variables: expected {}, got {}",
            scenario.expect.variables, state.variables
        ));
    }
    if trace != scenario.expect.trace {
        problems.push("trace mismatch".to_string());
        for i in 0..trace.len().max(scenario.expect.trace.len()) {
            let want = scenario
                .expect
                .trace
                .get(i)
                .map(String::as_str)
                .unwrap_or("<end>");
            let got = trace.get(i).map(String::as_str).unwrap_or("<end>");
            if want != got {
                problems.push(format!("  line {i}: expected `{want}`, got `{got}`"));
            }
        }
    }
    if !problems.is_empty() {
        writeln!(failures, "{name}:").unwrap();
        for p in problems {
            writeln!(failures, "  {p}").unwrap();
        }
    }
}

#[test]
fn scenario_corpus() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios");
    let mut paths: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();
    assert!(paths.len() >= 10, "scenario corpus unexpectedly small");

    let mut failures = String::new();
    for path in &paths {
        run_scenario(path, &mut failures);
    }
    assert!(failures.is_empty(), "\n{failures}");
}
