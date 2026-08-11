//! Property tests (design brief, testing strategy #3): the invariants that
//! make local token counting trustworthy, checked across interleavings.

use proptest::prelude::*;
use rbpmn_core::*;
use serde_json::json;
use std::fs;
use std::path::Path;

fn compile(fixture: &str) -> ExecutableProcess {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../rbpmn-model/tests/fixtures")
        .join(fixture);
    let defs = rbpmn_model::parse(&fs::read_to_string(path).unwrap()).unwrap();
    ExecutableProcess::compile(&defs, "p", &Bindings::default()).unwrap()
}

/// Run to completion, always completing the open work item whose element id
/// ranks first under `priority`. Returns the full trace.
fn run_with_priority(
    proc: &ExecutableProcess,
    priority: &[String],
    mut patch_for: impl FnMut(&str) -> serde_json::Value,
) -> (InstanceState, Vec<String>) {
    let mut state = InstanceState::new();
    let mut trace: Vec<String> = step(
        proc,
        &mut state,
        Command::Start {
            variables: json!({}),
        },
    )
    .unwrap()
    .iter()
    .map(|e| e.to_string())
    .collect();

    while state.status == InstanceStatus::Active {
        let mut open: Vec<(WorkItemId, String)> = state
            .open_work_items()
            .map(|(id, w)| (id, proc.node_id(w.element).to_string()))
            .collect();
        assert!(
            !open.is_empty(),
            "active instance with nothing to do — deadlock"
        );
        open.sort_by_key(|(_, element)| {
            priority
                .iter()
                .position(|p| p == element)
                .unwrap_or(usize::MAX)
        });
        let (id, element) = open.remove(0);
        let events = step(
            proc,
            &mut state,
            Command::CompleteWorkItem {
                id,
                patch: patch_for(&element),
            },
        )
        .unwrap();
        trace.extend(events.iter().map(|e| e.to_string()));

        // Quiescence invariant after every step: tokens parked at joins or
        // behind open work items, one-to-one.
        let work_waits = state
            .tokens()
            .filter(|(_, t)| matches!(t.wait, WaitKind::WorkItem(_)))
            .count();
        assert_eq!(work_waits, state.open_work_items().count());
    }
    (state, trace)
}

proptest! {
    /// Any completion order of the nested-parallel model: instance completes,
    /// zero runtime state remains, every join fires exactly once, and the
    /// event *multiset* is identical (confluence).
    #[test]
    fn nested_parallel_is_confluent(order in Just(vec![
        "ta".to_string(), "tb".to_string(), "tc".to_string()
    ]).prop_shuffle()) {
        let proc = compile("accept/04-nested-parallel.bpmn");
        let (state, trace) = run_with_priority(&proc, &order, |_| json!({}));

        prop_assert_eq!(state.status, InstanceStatus::Completed);
        prop_assert_eq!(state.tokens().count(), 0);
        prop_assert_eq!(state.open_work_items().count(), 0);
        for join in ["j1", "j2"] {
            let fired = trace.iter().filter(|l| **l == format!("element-started {join}")).count();
            prop_assert_eq!(fired, 1, "join {} fired {} times", join, fired);
        }

        let baseline = run_with_priority(
            &proc,
            &["ta".to_string(), "tb".to_string(), "tc".to_string()],
            |_| json!({}),
        ).1;
        let mut a = trace.clone();
        let mut b = baseline.clone();
        a.sort();
        b.sort();
        prop_assert_eq!(a, b, "event multiset differs across completion orders");
    }

    /// Concurrent branches patch disjoint variables: both patches land
    /// regardless of completion order (what merge-patch is for).
    #[test]
    fn concurrent_patches_both_land(order in Just(vec![
        "ta".to_string(), "tb".to_string()
    ]).prop_shuffle()) {
        let proc = compile("accept/03-parallel-gateway.bpmn");
        let (state, _) = run_with_priority(&proc, &order, |element| json!({ element: "done" }));
        prop_assert_eq!(state.status, InstanceStatus::Completed);
        prop_assert_eq!(&state.variables, &json!({ "ta": "done", "tb": "done" }));
    }

    /// The terminate race: whichever branch order, a cancelled=true pass
    /// through the checker terminates the instance and leaves zero runtime
    /// state; cancelled=false completes it normally.
    #[test]
    fn terminate_race_is_clean(
        tb_first in any::<bool>(),
        cancelled in any::<bool>(),
    ) {
        let proc = compile("accept/12-terminate-race.bpmn");
        let order = if tb_first { vec!["tb".to_string(), "ta".to_string()] } else { vec!["ta".to_string(), "tb".to_string()] };
        let (state, _) = run_with_priority(&proc, &order, |element| {
            if element == "tb" { json!({ "cancelled": cancelled }) } else { json!({}) }
        });

        if cancelled && tb_first {
            // tb ran first: the terminate fires while ta is still open.
            prop_assert_eq!(state.status, InstanceStatus::Terminated);
        } else if cancelled {
            // ta completed before the terminate — the join is starved but the
            // terminate still kills the instance when tb's branch reaches it.
            prop_assert_eq!(state.status, InstanceStatus::Terminated);
        } else {
            prop_assert_eq!(state.status, InstanceStatus::Completed);
        }
        prop_assert_eq!(state.tokens().count(), 0);
        prop_assert_eq!(state.open_work_items().count(), 0);
    }

    /// Loop around a parallel block: any bounded number of iterations
    /// completes, and the block executes exactly n+1 times.
    #[test]
    fn loop_iterations_are_exact(n in 0usize..4) {
        let proc = compile("accept/06-loop-around-block.bpmn");
        let mut remaining = n;
        let (state, trace) = run_with_priority(
            &proc,
            &["ta".to_string(), "tb".to_string()],
            |element| {
                if element == "tb" {
                    let again = remaining > 0;
                    remaining = remaining.saturating_sub(1);
                    json!({ "again": again })
                } else {
                    json!({})
                }
            },
        );
        prop_assert_eq!(state.status, InstanceStatus::Completed);
        let splits = trace.iter().filter(|l| **l == "element-started ps").count();
        prop_assert_eq!(splits, n + 1);
    }
}

fn compile_with(fixture: &str, bindings: &Bindings) -> ExecutableProcess {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../rbpmn-model/tests/fixtures")
        .join(fixture);
    let defs = rbpmn_model::parse(&fs::read_to_string(path).unwrap()).unwrap();
    ExecutableProcess::compile(&defs, "p", bindings).unwrap()
}

proptest! {
    /// The event-based gateway race, across every stimulus order: whichever
    /// of the three armed events arrives first wins, the losers' handles
    /// answer typed errors (their arms were withdrawn with the race), the
    /// instance completes via exactly the winner's end event, and zero
    /// timers/subscriptions survive.
    #[test]
    fn event_gateway_race_is_exclusive(order in Just(vec![
        "c_paid".to_string(), "c_cancel".to_string(), "c_late".to_string()
    ]).prop_shuffle()) {
        let bindings = Bindings::new()
            .correlation("c_paid", "order.id")
            .correlation("c_cancel", "order.id");
        let proc = compile_with("accept/11-event-based-gateway.bpmn", &bindings);
        let mut state = InstanceState::new();
        step(&proc, &mut state, Command::Start {
            variables: json!({"order": {"id": "o-race"}}),
        }).unwrap();

        // Capture every armed handle before the race resolves.
        let timer = state.armed_timer_at(proc.node_by_id("c_late").unwrap()).unwrap();
        let subs: Vec<(String, SubscriptionId)> = ["c_paid", "c_cancel"]
            .iter()
            .map(|el| {
                let id = state
                    .armed_subscription_at(proc.node_by_id(el).unwrap())
                    .unwrap();
                (el.to_string(), id)
            })
            .collect();
        let command_for = |element: &str| -> Command {
            if element == "c_late" {
                Command::FireTimer { id: timer }
            } else {
                let (_, id) = subs.iter().find(|(el, _)| el == element).unwrap();
                Command::DeliverMessage { id: *id, patch: json!({}) }
            }
        };

        let winner = &order[0];
        let events = step(&proc, &mut state, command_for(winner)).unwrap();
        prop_assert_eq!(state.status, InstanceStatus::Completed);
        prop_assert_eq!(state.timers().count(), 0);
        prop_assert_eq!(state.subscriptions().count(), 0);

        // Exactly the winner's end event, no other.
        let trace: Vec<String> = events.iter().map(|e| e.to_string()).collect();
        let expected_end = match winner.as_str() {
            "c_paid" => "e_paid",
            "c_cancel" => "e_cancel",
            _ => "e_late",
        };
        for end in ["e_paid", "e_cancel", "e_late"] {
            let started = trace.iter().filter(|l| **l == format!("element-started {end}")).count();
            prop_assert_eq!(started, usize::from(end == expected_end), "{}: {:?}", end, trace);
        }

        // The losers' handles are gone — typed, never a silent no-op.
        for loser in &order[1..] {
            let err = step(&proc, &mut state, command_for(loser));
            prop_assert!(matches!(
                err,
                Err(StepError::InstanceNotActive(_))
                    | Err(StepError::UnknownTimer(_))
                    | Err(StepError::UnknownSubscription(_))
            ), "loser {} got {:?}", loser, err);
        }
    }

    /// The boundary-timer race on a task: completion and timeout in either
    /// order — the first resolves the task, the second answers a typed
    /// error, and the instance always completes with zero runtime state.
    #[test]
    fn boundary_timer_race_is_exclusive(timeout_first in any::<bool>()) {
        let proc = compile("accept/09-timer-boundary.bpmn");
        let mut state = InstanceState::new();
        step(&proc, &mut state, Command::Start { variables: json!({}) }).unwrap();

        let item = state
            .open_work_item_at(proc.node_by_id("ut").unwrap())
            .unwrap();
        let timer = state.armed_timer_at(proc.node_by_id("bt").unwrap()).unwrap();

        if timeout_first {
            step(&proc, &mut state, Command::FireTimer { id: timer }).unwrap();
            // The interrupted task's item is closed: typed not-open.
            let err = step(&proc, &mut state, Command::CompleteWorkItem {
                id: item, patch: json!({}),
            });
            prop_assert_eq!(err, Err(StepError::WorkItemNotOpen(item)));
            // Escalation path still needs its user task.
            let esc = state
                .open_work_item_at(proc.node_by_id("t_esc").unwrap())
                .unwrap();
            step(&proc, &mut state, Command::CompleteWorkItem {
                id: esc, patch: json!({}),
            }).unwrap();
        } else {
            step(&proc, &mut state, Command::CompleteWorkItem {
                id: item, patch: json!({}),
            }).unwrap();
            // The completed task's boundary timer was disarmed with it.
            let err = step(&proc, &mut state, Command::FireTimer { id: timer });
            prop_assert!(matches!(
                err,
                Err(StepError::InstanceNotActive(_)) | Err(StepError::UnknownTimer(_))
            ), "{:?}", err);
        }

        prop_assert_eq!(state.status, InstanceStatus::Completed);
        prop_assert_eq!(state.tokens().count(), 0);
        prop_assert_eq!(state.timers().count(), 0);
        prop_assert_eq!(state.subscriptions().count(), 0);
    }
}
