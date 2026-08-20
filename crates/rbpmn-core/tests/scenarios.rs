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

// deny_unknown_fields on each shape: a stray field (a patch on a fail, a
// code on a complete) is a loud scenario error, never silent data loss.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompleteAction {
    complete: String,
    #[serde(default)]
    patch: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FailAction {
    fail: String,
    #[serde(default)]
    code: Option<String>,
}

/// Fire the armed timer sitting on this element — time as command data:
/// scenarios "advance the clock" by simply saying so, which is what makes
/// the years-long-sleep fixture run without any clock existing.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FireAction {
    fire: String,
}

/// Deliver a message to the open subscription on this element.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeliverAction {
    deliver: String,
    #[serde(default)]
    patch: Option<Value>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Action {
    Complete(CompleteAction),
    Decide(DecideAction),
    Fail(FailAction),
    Fire(FireAction),
    Deliver(DeliverAction),
}

/// Answer the decision a business-rule task is waiting on.
///
/// The answer is *given*, not computed — which is the whole point of the
/// design: the core cannot evaluate anything, so a golden trace for the
/// decision path needs no evaluator, and a replay reads the recorded answer
/// exactly this way (`docs/dmn.md`, D3).
#[derive(Deserialize)]
struct DecideAction {
    decide: String,
    answer: Option<Value>,
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
        let (element, command) = match action {
            Action::Complete(CompleteAction { complete, patch }) => {
                let node = proc
                    .node_by_id(complete)
                    .unwrap_or_else(|| panic!("{name}: no element '{complete}'"));
                let id = state
                    .open_work_item_at(node)
                    .unwrap_or_else(|| panic!("{name}: no open work item at '{complete}'"));
                let patch = patch.clone().unwrap_or_else(|| serde_json::json!({}));
                (complete, Command::CompleteWorkItem { id, patch })
            }
            Action::Decide(DecideAction { decide, answer }) => {
                let node = proc
                    .node_by_id(decide)
                    .unwrap_or_else(|| panic!("{name}: no element '{decide}'"));
                let token = state
                    .tokens()
                    .find(|(_, t)| t.node == node && t.wait == rbpmn_core::WaitKind::Decision)
                    .map(|(id, _)| id)
                    .unwrap_or_else(|| panic!("{name}: nothing awaits a decision at '{decide}'"));
                (
                    decide,
                    Command::CompleteDecision {
                        token,
                        answer: answer.clone(),
                        // Scenarios pin control flow and the variable
                        // document; the evaluator's prose is engine-side and
                        // has no bearing on either.
                        reason: None,
                    },
                )
            }
            Action::Fail(FailAction { fail, code }) => {
                let node = proc
                    .node_by_id(fail)
                    .unwrap_or_else(|| panic!("{name}: no element '{fail}'"));
                let id = state
                    .open_work_item_at(node)
                    .unwrap_or_else(|| panic!("{name}: no open work item at '{fail}'"));
                (
                    fail,
                    Command::RaiseError {
                        id,
                        code: code.clone(),
                    },
                )
            }
            Action::Fire(FireAction { fire }) => {
                let node = proc
                    .node_by_id(fire)
                    .unwrap_or_else(|| panic!("{name}: no element '{fire}'"));
                let id = state
                    .armed_timer_at(node)
                    .unwrap_or_else(|| panic!("{name}: no armed timer at '{fire}'"));
                (fire, Command::FireTimer { id })
            }
            Action::Deliver(DeliverAction { deliver, patch }) => {
                let node = proc
                    .node_by_id(deliver)
                    .unwrap_or_else(|| panic!("{name}: no element '{deliver}'"));
                let id = state
                    .armed_subscription_at(node)
                    .unwrap_or_else(|| panic!("{name}: no open subscription at '{deliver}'"));
                let patch = patch.clone().unwrap_or_else(|| serde_json::json!({}));
                (deliver, Command::DeliverMessage { id, patch })
            }
        };
        let events = step(&proc, &mut state, command)
            .unwrap_or_else(|e| panic!("{name}: action on '{element}' failed: {e}"));
        trace.extend(events.iter().map(|e| e.to_string()));
    }

    let status = match state.status {
        InstanceStatus::Created => "created",
        InstanceStatus::Active => "active",
        InstanceStatus::Completed => "completed",
        InstanceStatus::Terminated => "terminated",
        InstanceStatus::Failed => "failed",
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
