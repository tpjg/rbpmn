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
