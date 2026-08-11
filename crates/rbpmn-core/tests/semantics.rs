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
    // Message start/throw events lint clean (in the v1 model surface) but
    // stay un-executable until cross-definition messaging lands.
    let throwing = ExecutableProcess::compile(
        &load("accept/08-message-events.bpmn"),
        "p",
        &Bindings::default(),
    );
    match throwing {
        Err(CompileError::NotYetExecutable { element, phase, .. }) => {
            assert_eq!(element, "s_msg");
            assert!(phase.contains("correlate"), "{phase}");
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
fn compile_requires_correlation_bindings() {
    // `message-has-correlation`: no binding, no compile — never a default.
    let unmapped = ExecutableProcess::compile(
        &load("accept/17-message-catch.bpmn"),
        "p",
        &Bindings::default(),
    );
    match unmapped {
        Err(CompileError::MissingCorrelation(elements)) => {
            assert_eq!(elements, vec!["c".to_string()]);
        }
        other => panic!("expected MissingCorrelation, got {other:?}"),
    }

    // The binding must be a FEEL qualified name, not arbitrary text.
    let invalid = ExecutableProcess::compile(
        &load("accept/17-message-catch.bpmn"),
        "p",
        &Bindings::new().correlation("c", "order..id"),
    );
    match invalid {
        Err(CompileError::InvalidCorrelation { element, .. }) => assert_eq!(element, "c"),
        other => panic!("expected InvalidCorrelation, got {other:?}"),
    }
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

// ---------------------------------------------------------------------------
// Incident freeze: one uniform shape (review round after phase 3)
// ---------------------------------------------------------------------------

/// Event-based gateway with the timer armed *before* the failing message
/// alternative: the partial arm must be withdrawn and the token parked at
/// the failing element as `Incident` — never a Failed instance with live
/// arms or no token at all.
#[test]
fn gateway_correlation_incident_withdraws_partial_arms() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL"
                  id="defs" targetNamespace="urn:test">
  <bpmn:message id="m" name="Answer"/>
  <bpmn:process id="p" isExecutable="true">
    <bpmn:startEvent id="start"/>
    <bpmn:eventBasedGateway id="ebg"/>
    <bpmn:intermediateCatchEvent id="c_t">
      <bpmn:timerEventDefinition><bpmn:timeDuration>P1D</bpmn:timeDuration></bpmn:timerEventDefinition>
    </bpmn:intermediateCatchEvent>
    <bpmn:intermediateCatchEvent id="c_m">
      <bpmn:messageEventDefinition messageRef="m"/>
    </bpmn:intermediateCatchEvent>
    <bpmn:endEvent id="e_t"/>
    <bpmn:endEvent id="e_m"/>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="ebg"/>
    <bpmn:sequenceFlow id="f2" sourceRef="ebg" targetRef="c_t"/>
    <bpmn:sequenceFlow id="f3" sourceRef="ebg" targetRef="c_m"/>
    <bpmn:sequenceFlow id="f4" sourceRef="c_t" targetRef="e_t"/>
    <bpmn:sequenceFlow id="f5" sourceRef="c_m" targetRef="e_m"/>
  </bpmn:process>
</bpmn:definitions>"#;
    let defs = rbpmn_model::parse(xml).unwrap();
    let bindings = Bindings::new().correlation("c_m", "order.id");
    let proc = ExecutableProcess::compile(&defs, "p", &bindings).unwrap();
    let mut state = InstanceState::new();
    let events = step(
        &proc,
        &mut state,
        Command::Start {
            variables: json!({}), // no order.id: c_m's arming fails
        },
    )
    .unwrap();

    let trace: Vec<String> = events.iter().map(|e| e.to_string()).collect();
    assert!(
        trace.contains(&"timer-armed c_t P1D".to_string()),
        "{trace:?}"
    );
    assert!(trace.contains(&"correlation-failed c_m order.id".to_string()));
    assert!(
        trace.contains(&"timer-cancelled c_t".to_string()),
        "the partial arm must be withdrawn: {trace:?}"
    );
    assert_eq!(*trace.last().unwrap(), "incident-raised c_m".to_string());

    assert_eq!(state.status, InstanceStatus::Failed);
    assert_eq!(state.timers().count(), 0);
    assert_eq!(state.subscriptions().count(), 0);
    let tokens: Vec<_> = state.tokens().collect();
    assert_eq!(tokens.len(), 1, "the token must survive the freeze");
    assert_eq!(tokens[0].1.node, proc.node_by_id("c_m").unwrap());
    assert_eq!(tokens[0].1.wait, WaitKind::Incident);
}

/// Floats have no canonical spelling across a jsonb round-trip; only strings
/// and exact integers are valid correlation keys.
#[test]
fn float_correlation_keys_are_rejected_loudly() {
    let defs = load("accept/17-message-catch.bpmn");
    let bindings = Bindings::new().correlation("c", "order.id");
    let proc = ExecutableProcess::compile(&defs, "p", &bindings).unwrap();

    // Exact integer: fine, canonical rendering.
    let mut state = InstanceState::new();
    let events = step(
        &proc,
        &mut state,
        Command::Start {
            variables: json!({"order": {"id": 84231}}),
        },
    )
    .unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.to_string() == "message-subscribed c WarehouseAck 84231")
    );

    // Float: incident, token parked at the catch.
    let mut state = InstanceState::new();
    let events = step(
        &proc,
        &mut state,
        Command::Start {
            variables: json!({"order": {"id": 1.5}}),
        },
    )
    .unwrap();
    assert_eq!(state.status, InstanceStatus::Failed);
    assert!(
        events
            .iter()
            .any(|e| e.to_string() == "correlation-failed c order.id")
    );
    assert_eq!(state.tokens().next().unwrap().1.wait, WaitKind::Incident);
}

/// Two open subscriptions for one (message, key) would make every delivery
/// permanently ambiguous — arming the second freezes the instance instead.
#[test]
fn duplicate_subscription_freezes_instead_of_arming_ambiguity() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL"
                  id="defs" targetNamespace="urn:test">
  <bpmn:message id="m" name="Answer"/>
  <bpmn:process id="p" isExecutable="true">
    <bpmn:startEvent id="start"/>
    <bpmn:parallelGateway id="ps"/>
    <bpmn:intermediateCatchEvent id="c_a">
      <bpmn:messageEventDefinition messageRef="m"/>
    </bpmn:intermediateCatchEvent>
    <bpmn:intermediateCatchEvent id="c_b">
      <bpmn:messageEventDefinition messageRef="m"/>
    </bpmn:intermediateCatchEvent>
    <bpmn:parallelGateway id="pj"/>
    <bpmn:endEvent id="end"/>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="ps"/>
    <bpmn:sequenceFlow id="f2" sourceRef="ps" targetRef="c_a"/>
    <bpmn:sequenceFlow id="f3" sourceRef="ps" targetRef="c_b"/>
    <bpmn:sequenceFlow id="f4" sourceRef="c_a" targetRef="pj"/>
    <bpmn:sequenceFlow id="f5" sourceRef="c_b" targetRef="pj"/>
    <bpmn:sequenceFlow id="f6" sourceRef="pj" targetRef="end"/>
  </bpmn:process>
</bpmn:definitions>"#;
    let defs = rbpmn_model::parse(xml).unwrap();
    let bindings = Bindings::new()
        .correlation("c_a", "order.id")
        .correlation("c_b", "order.id");
    let proc = ExecutableProcess::compile(&defs, "p", &bindings).unwrap();
    let mut state = InstanceState::new();
    let events = step(
        &proc,
        &mut state,
        Command::Start {
            variables: json!({"order": {"id": "o-1"}}),
        },
    )
    .unwrap();

    assert_eq!(state.status, InstanceStatus::Failed);
    let trace: Vec<String> = events.iter().map(|e| e.to_string()).collect();
    assert!(
        trace.contains(&"duplicate-subscription c_b Answer o-1".to_string()),
        "{trace:?}"
    );
    // The first, legitimate subscription is withdrawn by the freeze? No —
    // it belongs to a *different* token, which stays frozen as-is; what
    // matters is that no deliverable ambiguity exists and the failure is
    // attributed to the second catch.
    assert!(trace.contains(&"message-subscribed c_a Answer o-1".to_string()));
}

/// The error-boundary incident converges on the same freeze shape: the
/// token reparked at the failed task as `Incident`.
#[test]
fn error_incident_parks_the_token_at_the_failed_task() {
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
    step(&proc, &mut state, Command::RaiseError { id, code: None }).unwrap();

    assert_eq!(state.status, InstanceStatus::Failed);
    let tokens: Vec<_> = state.tokens().collect();
    assert_eq!(tokens.len(), 1);
    assert_eq!(tokens[0].1.node, proc.node_by_id("review").unwrap());
    assert_eq!(tokens[0].1.wait, WaitKind::Incident);
}

/// Several timer boundaries on one task: the first to fire wins, interrupts
/// the task, and withdraws its siblings — first-fires-wins, exactly one
/// continuation.
#[test]
fn multiple_timer_boundaries_first_fires_wins() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL"
                  id="defs" targetNamespace="urn:test">
  <bpmn:process id="p" isExecutable="true">
    <bpmn:startEvent id="start"/>
    <bpmn:userTask id="ut"/>
    <bpmn:boundaryEvent id="bt_warn" attachedToRef="ut">
      <bpmn:timerEventDefinition><bpmn:timeDuration>PT1H</bpmn:timeDuration></bpmn:timerEventDefinition>
    </bpmn:boundaryEvent>
    <bpmn:boundaryEvent id="bt_hard" attachedToRef="ut">
      <bpmn:timerEventDefinition><bpmn:timeDuration>P1D</bpmn:timeDuration></bpmn:timerEventDefinition>
    </bpmn:boundaryEvent>
    <bpmn:endEvent id="end"/>
    <bpmn:endEvent id="e_warn"/>
    <bpmn:endEvent id="e_hard"/>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="ut"/>
    <bpmn:sequenceFlow id="f2" sourceRef="ut" targetRef="end"/>
    <bpmn:sequenceFlow id="f3" sourceRef="bt_warn" targetRef="e_warn"/>
    <bpmn:sequenceFlow id="f4" sourceRef="bt_hard" targetRef="e_hard"/>
  </bpmn:process>
</bpmn:definitions>"#;
    let defs = rbpmn_model::parse(xml).unwrap();
    let proc = ExecutableProcess::compile(&defs, "p", &Bindings::default()).unwrap();

    // Fire the *second* boundary: the first is withdrawn with the host.
    let mut state = InstanceState::new();
    step(
        &proc,
        &mut state,
        Command::Start {
            variables: json!({}),
        },
    )
    .unwrap();
    assert_eq!(state.timers().count(), 2);
    let hard = state
        .armed_timer_at(proc.node_by_id("bt_hard").unwrap())
        .unwrap();
    let events = step(&proc, &mut state, Command::FireTimer { id: hard }).unwrap();
    let trace: Vec<String> = events.iter().map(|e| e.to_string()).collect();
    assert!(
        trace.contains(&"work-item-cancelled ut".to_string()),
        "{trace:?}"
    );
    assert!(
        trace.contains(&"timer-cancelled bt_warn".to_string()),
        "{trace:?}"
    );
    assert!(trace.contains(&"element-started e_hard".to_string()));
    assert!(!trace.contains(&"element-started e_warn".to_string()));
    assert_eq!(state.status, InstanceStatus::Completed);
    assert_eq!(state.timers().count(), 0);

    // Completing the task disarms both.
    let mut state = InstanceState::new();
    step(
        &proc,
        &mut state,
        Command::Start {
            variables: json!({}),
        },
    )
    .unwrap();
    let item = state
        .open_work_item_at(proc.node_by_id("ut").unwrap())
        .unwrap();
    let events = step(
        &proc,
        &mut state,
        Command::CompleteWorkItem {
            id: item,
            patch: json!({}),
        },
    )
    .unwrap();
    let trace: Vec<String> = events.iter().map(|e| e.to_string()).collect();
    assert!(
        trace.contains(&"timer-cancelled bt_warn".to_string()),
        "{trace:?}"
    );
    assert!(trace.contains(&"timer-cancelled bt_hard".to_string()));
    assert_eq!(state.status, InstanceStatus::Completed);
    assert_eq!(state.timers().count(), 0);
}

/// Token conservation across a mid-advance freeze: a parallel sibling still
/// queued when the incident fires must park (Incident wait at its target),
/// never silently vanish — a frozen instance that lost a branch could never
/// be repaired.
#[test]
fn freeze_parks_in_flight_sibling_tokens() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL"
                  id="defs" targetNamespace="urn:test">
  <bpmn:message id="m" name="Go"/>
  <bpmn:process id="p" isExecutable="true">
    <bpmn:startEvent id="start"/>
    <bpmn:parallelGateway id="ps"/>
    <bpmn:intermediateCatchEvent id="c">
      <bpmn:messageEventDefinition messageRef="m"/>
    </bpmn:intermediateCatchEvent>
    <bpmn:userTask id="ut"/>
    <bpmn:parallelGateway id="pj"/>
    <bpmn:endEvent id="end"/>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="ps"/>
    <bpmn:sequenceFlow id="f2" sourceRef="ps" targetRef="c"/>
    <bpmn:sequenceFlow id="f3" sourceRef="ps" targetRef="ut"/>
    <bpmn:sequenceFlow id="f4" sourceRef="c" targetRef="pj"/>
    <bpmn:sequenceFlow id="f5" sourceRef="ut" targetRef="pj"/>
    <bpmn:sequenceFlow id="f6" sourceRef="pj" targetRef="end"/>
  </bpmn:process>
</bpmn:definitions>"#;
    let defs = rbpmn_model::parse(xml).unwrap();
    // Branch f2 (declared first) fails its correlation while branch f3's
    // token is still queued behind it in the same advancement.
    let bindings = Bindings::new().correlation("c", "order.id");
    let proc = ExecutableProcess::compile(&defs, "p", &bindings).unwrap();
    let mut state = InstanceState::new();
    step(
        &proc,
        &mut state,
        Command::Start {
            variables: json!({}), // no order.id
        },
    )
    .unwrap();

    assert_eq!(state.status, InstanceStatus::Failed);
    let mut tokens: Vec<(usize, WaitKind)> = state
        .tokens()
        .map(|(_, t)| (t.node, t.wait.clone()))
        .collect();
    tokens.sort_by_key(|(node, _)| *node);
    assert_eq!(
        tokens.len(),
        2,
        "both branch tokens must survive the freeze"
    );
    let c = proc.node_by_id("c").unwrap();
    let ut = proc.node_by_id("ut").unwrap();
    assert!(tokens.contains(&(c, WaitKind::Incident)), "{tokens:?}");
    assert!(
        tokens.contains(&(ut, WaitKind::Incident)),
        "the in-flight sibling parks at its target: {tokens:?}"
    );
    // The sibling never entered its node: no work item was created for it.
    assert_eq!(state.open_work_items().count(), 0);
}
