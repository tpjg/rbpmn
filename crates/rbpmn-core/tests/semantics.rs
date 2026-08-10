//! Exhaustive semantic unit tests: typed error results, compile gating, and
//! the invariants the projection layer will rely on.

use rbpmn_core::*;
use serde_json::json;
use std::fs;
use std::path::Path;

fn load(fixture: &str) -> rbpmn_model::model::Definitions {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../rbpmn-model/tests/fixtures")
        .join(fixture);
    rbpmn_model::parse(&fs::read_to_string(path).unwrap()).unwrap()
}

fn compile(fixture: &str) -> ExecutableProcess {
    ExecutableProcess::compile(&load(fixture), "p", &Bindings::default()).unwrap()
}

fn work_item_at(proc: &ExecutableProcess, state: &InstanceState, element: &str) -> WorkItemId {
    state
        .open_work_item_at(proc.node_by_id(element).unwrap())
        .unwrap_or_else(|| panic!("no open work item at {element}"))
}

#[test]
fn complete_is_exactly_once() {
    let proc = compile("accept/01-minimal.bpmn");
    let mut state = InstanceState::new();
    step(
        &proc,
        &mut state,
        Command::Start {
            variables: json!({}),
        },
    )
    .unwrap();

    let id = work_item_at(&proc, &state, "review");
    step(
        &proc,
        &mut state,
        Command::CompleteWorkItem {
            id,
            patch: json!({}),
        },
    )
    .unwrap();

    // Second completion: the typed not-open result, not unknown, and the
    // state is untouched by the failed attempt.
    let before = state.clone();
    let err = step(
        &proc,
        &mut state,
        Command::CompleteWorkItem {
            id,
            patch: json!({}),
        },
    );
    assert_eq!(
        err,
        Err(StepError::InstanceNotActive(InstanceStatus::Completed))
    );
    assert_eq!(state, before);
}

#[test]
fn complete_twice_while_active_is_not_open() {
    let proc = compile("accept/03-parallel-gateway.bpmn");
    let mut state = InstanceState::new();
    step(
        &proc,
        &mut state,
        Command::Start {
            variables: json!({}),
        },
    )
    .unwrap();

    let ta = work_item_at(&proc, &state, "ta");
    step(
        &proc,
        &mut state,
        Command::CompleteWorkItem {
            id: ta,
            patch: json!({}),
        },
    )
    .unwrap();

    let before = state.clone();
    let err = step(
        &proc,
        &mut state,
        Command::CompleteWorkItem {
            id: ta,
            patch: json!({}),
        },
    );
    assert_eq!(err, Err(StepError::WorkItemNotOpen(ta)));
    assert_eq!(state, before);

    let unknown = step(
        &proc,
        &mut state,
        Command::CompleteWorkItem {
            id: WorkItemId(999),
            patch: json!({}),
        },
    );
    assert_eq!(unknown, Err(StepError::UnknownWorkItem(WorkItemId(999))));
}

#[test]
fn start_twice_is_rejected() {
    let proc = compile("accept/01-minimal.bpmn");
    let mut state = InstanceState::new();
    step(
        &proc,
        &mut state,
        Command::Start {
            variables: json!({}),
        },
    )
    .unwrap();
    let err = step(
        &proc,
        &mut state,
        Command::Start {
            variables: json!({}),
        },
    );
    assert_eq!(err, Err(StepError::AlreadyStarted));
}

#[test]
fn terminate_leaves_zero_runtime_state() {
    let proc = compile("accept/12-terminate-race.bpmn");
    let mut state = InstanceState::new();
    step(
        &proc,
        &mut state,
        Command::Start {
            variables: json!({}),
        },
    )
    .unwrap();
    let tb = work_item_at(&proc, &state, "tb");
    step(
        &proc,
        &mut state,
        Command::CompleteWorkItem {
            id: tb,
            patch: json!({ "cancelled": true }),
        },
    )
    .unwrap();

    assert_eq!(state.status, InstanceStatus::Terminated);
    assert_eq!(state.tokens().count(), 0);
    assert_eq!(state.open_work_items().count(), 0);

    // The cancelled item answers "not open", not "unknown" — but the
    // instance gate comes first.
    let ta_ids: Vec<WorkItemId> = state.work_items().map(|(id, _)| id).collect();
    for id in ta_ids {
        let err = step(
            &proc,
            &mut state,
            Command::CompleteWorkItem {
                id,
                patch: json!({}),
            },
        );
        assert_eq!(
            err,
            Err(StepError::InstanceNotActive(InstanceStatus::Terminated))
        );
    }
}

#[test]
fn quiescent_state_is_consistent_after_every_step() {
    let proc = compile("accept/04-nested-parallel.bpmn");
    let mut state = InstanceState::new();
    step(
        &proc,
        &mut state,
        Command::Start {
            variables: json!({}),
        },
    )
    .unwrap();

    for element in ["ta", "tb", "tc"] {
        // Every token is at a wait position; work-item waits point at open
        // items that point back at the same token.
        for (token_id, token) in state.tokens() {
            if let WaitKind::WorkItem(id) = token.wait {
                let (_, item) = state.work_items().find(|(i, _)| *i == id).unwrap();
                assert!(item.open);
                assert_eq!(item.token, token_id);
                assert_eq!(item.element, token.node);
            }
        }
        let id = work_item_at(&proc, &state, element);
        step(
            &proc,
            &mut state,
            Command::CompleteWorkItem {
                id,
                patch: json!({}),
            },
        )
        .unwrap();
    }
    assert_eq!(state.status, InstanceStatus::Completed);
    assert_eq!(state.tokens().count(), 0);
}

#[test]
fn compile_gates_later_phase_elements() {
    let receive = ExecutableProcess::compile(
        &load("accept/07-task-kinds.bpmn"),
        "p",
        &Bindings::default(),
    );
    match receive {
        Err(CompileError::NotYetExecutable { element, phase, .. }) => {
            assert_eq!(element, "rt");
            assert!(phase.contains("phase 3"), "{phase}");
        }
        other => panic!("expected NotYetExecutable, got {other:?}"),
    }

    let subprocess = ExecutableProcess::compile(
        &load("accept/13-subprocess.bpmn"),
        "p",
        &Bindings::default(),
    );
    assert!(matches!(
        subprocess,
        Err(CompileError::NotYetExecutable { .. })
    ));
}

#[test]
fn compile_rejects_lint_dirty_models() {
    let err = ExecutableProcess::compile(
        &load("reject/inclusive-gateway.bpmn"),
        "p",
        &Bindings::default(),
    );
    match err {
        Err(CompileError::RejectedByLint(diags)) => {
            assert!(
                diags
                    .iter()
                    .all(|d| d.severity == rbpmn_model::Severity::Error)
            );
            assert!(diags.iter().any(|d| d.rule == "no-inclusive-gateway"));
        }
        other => panic!("expected RejectedByLint, got {other:?}"),
    }
}

#[test]
fn topic_binding_defaults_to_element_id() {
    let proc = compile("accept/16-foreign-binding-warn.bpmn");
    let mut state = InstanceState::new();
    let events = step(
        &proc,
        &mut state,
        Command::Start {
            variables: json!({}),
        },
    )
    .unwrap();
    assert!(events.iter().any(|e| matches!(
        e,
        Event::WorkItemCreated { element, topic, .. } if element == "st" && topic == "st"
    )));
}

#[test]
fn manifest_builder_and_json_agree() {
    // The Rust builder and the server's JSON deploy body are two syntaxes
    // for one manifest: identical structs, identical compile result.
    let built = Bindings::new().topic("st", "payments");
    let parsed: Bindings = serde_json::from_str(r#"{ "topics": { "st": "payments" } }"#).unwrap();
    assert_eq!(built, parsed);

    let defs = load("accept/16-foreign-binding-warn.bpmn");
    for bindings in [&built, &parsed] {
        let proc = ExecutableProcess::compile(&defs, "p", bindings).unwrap();
        let mut state = InstanceState::new();
        let events = step(
            &proc,
            &mut state,
            Command::Start {
                variables: json!({}),
            },
        )
        .unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            Event::WorkItemCreated { topic, .. } if topic == "payments"
        )));
    }

    // An empty/omitted bindings object is valid and falls back to defaults.
    let empty: Bindings = serde_json::from_str("{}").unwrap();
    assert_eq!(empty, Bindings::default());
}
