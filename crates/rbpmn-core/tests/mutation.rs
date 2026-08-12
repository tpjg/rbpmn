//! Mutation fuzz and restriction counterexamples (docs/stress-testing.md
//! §3c and §3d) — the hunt for **outcome 1**: a model that lints clean and
//! then executes with wrong token semantics. That is the Camunda-lineage bug
//! class this engine's whole design exists to prevent, and it is the one
//! third outcome nothing else looks for.
//!
//! §3c takes a valid generated model, breaks it structurally in one place,
//! and asserts the dichotomy holds:
//!
//! ```text
//! linter rejects  -> fine, but the rule id must be a real, catalogued one
//! linter accepts  -> execute it exhaustively; ANY invariant violation,
//!                    `Internal`, or `StepError::Invariant` is a linter hole
//! ```
//!
//! A mutant that survives *both* is not a failure: the linter is allowed to
//! be permissive wherever the semantics stay correct.
//!
//! §3d runs the inverse. The `reject/` fixtures are exactly the
//! non-block-structured shapes, so compiling them with the lint gate off and
//! executing them shows the concrete hazard each rule prevents — turning
//! `balanced-gateways` from a restriction we assert is necessary into one
//! with a reproducible counterexample attached.

mod explorer;
mod modelgen;

use modelgen::{Block, Builder, Kind, Rng, build};
use proptest::prelude::*;
use rbpmn_core::{Bindings, CompileError, ExecutableProcess};
use serde_json::json;
use std::fs;
use std::path::Path;

/// What became of a model we deliberately broke.
#[derive(Debug, PartialEq)]
enum Outcome {
    /// The linter refused it — the intended path.
    Rejected,
    /// Lint-clean and executes with every invariant intact. Fine: the linter
    /// is allowed to be permissive where semantics stay correct.
    Survived,
    /// Lint-clean and then *wrong*. This is outcome 1.
    Hazard(String),
    /// Could not be judged: the state space outgrew the budget.
    Inconclusive,
}

/// Compile (gate already passed) and explore exhaustively.
fn execute(xml: &str) -> Outcome {
    let defs = match rbpmn_model::parse(xml) {
        Ok(d) => d,
        // Lint ran on a parsed document, so this cannot happen.
        Err(e) => return Outcome::Hazard(format!("lint-clean model failed to parse: {e}")),
    };
    let proc = match ExecutableProcess::compile(&defs, "p", &Bindings::default()) {
        Ok(p) => p,
        // "lint should have prevented this" — by its own words, a linter hole.
        Err(CompileError::Internal(m)) => {
            return Outcome::Hazard(format!("CompileError::Internal: {m}"));
        }
        Err(CompileError::RejectedByLint(d)) => {
            return Outcome::Hazard(format!(
                "compile re-lint disagreed with lint: {}",
                d.iter()
                    .map(|d| d.rule.clone())
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
        // Outside the executable subset, or no such process: not a semantic
        // hazard, just nothing to run.
        Err(_) => return Outcome::Inconclusive,
    };
    let report = explorer::explore(&proc, json!({}), &[]);
    if !report.violations.is_empty() {
        return Outcome::Hazard(report.violations.join("; "));
    }
    if report.capped {
        return Outcome::Inconclusive;
    }
    Outcome::Survived
}

/// Lint, then execute if the linter allows it.
fn judge(xml: &str) -> Outcome {
    let checked = match rbpmn_model::check(xml) {
        Ok(c) => c,
        Err(_) => return Outcome::Rejected, // unparseable is a loud refusal too
    };
    if !checked.ok {
        // Rule ids are stable public API: a mutant must never provoke an
        // unknown or empty one.
        for d in checked
            .diagnostics
            .iter()
            .filter(|d| d.severity == rbpmn_model::Severity::Error)
        {
            assert!(
                rbpmn_model::CATALOGUE.iter().any(|r| r.id == d.rule),
                "diagnostic '{}' is not in the published rule catalogue",
                d.rule
            );
        }
        return Outcome::Rejected;
    }
    execute(xml)
}

// ------------------------------------------------------------------ mutations

/// Structural mutations, each breaking block structure in one place. Every one
/// returns false when it does not apply to this model, so the caller can try
/// another rather than silently testing nothing.
type Mutation = fn(&mut Builder, &mut Rng) -> bool;

fn pick_flow(b: &Builder, rng: &mut Rng) -> Option<usize> {
    (!b.flows.is_empty()).then(|| rng.below(b.flows.len()))
}

/// Point a flow somewhere else entirely — the "branch escapes its block" case.
fn retarget_flow(b: &mut Builder, rng: &mut Rng) -> bool {
    let Some(i) = pick_flow(b, rng) else {
        return false;
    };
    let candidates: Vec<String> = b
        .elements
        .iter()
        .filter(|e| e.kind != Kind::Start && e.id != b.flows[i].target)
        .map(|e| e.id.clone())
        .collect();
    if candidates.is_empty() {
        return false;
    }
    b.flows[i].target = candidates[rng.below(candidates.len())].clone();
    true
}

/// Move a flow's source — entry into the middle of a region.
fn resource_flow(b: &mut Builder, rng: &mut Rng) -> bool {
    let Some(i) = pick_flow(b, rng) else {
        return false;
    };
    let candidates: Vec<String> = b
        .elements
        .iter()
        .filter(|e| e.kind != Kind::End && e.id != b.flows[i].source)
        .map(|e| e.id.clone())
        .collect();
    if candidates.is_empty() {
        return false;
    }
    b.flows[i].source = candidates[rng.below(candidates.len())].clone();
    true
}

/// Delete a flow: an orphaned join, or an unreachable tail.
fn drop_flow(b: &mut Builder, rng: &mut Rng) -> bool {
    let Some(i) = pick_flow(b, rng) else {
        return false;
    };
    b.flows.remove(i);
    true
}

/// Give a task a second outgoing flow — the implicit split the spec would
/// read as a parallel (or inclusive!) fork.
fn implicit_split(b: &mut Builder, rng: &mut Rng) -> bool {
    let tasks: Vec<String> = b
        .elements
        .iter()
        .filter(|e| e.kind == Kind::UserTask)
        .map(|e| e.id.clone())
        .collect();
    if tasks.is_empty() {
        return false;
    }
    let source = tasks[rng.below(tasks.len())].clone();
    let targets: Vec<String> = b
        .elements
        .iter()
        .filter(|e| e.kind != Kind::Start && e.id != source)
        .map(|e| e.id.clone())
        .collect();
    if targets.is_empty() {
        return false;
    }
    let target = targets[rng.below(targets.len())].clone();
    let id = format!("fx{}", b.flows.len() + 1);
    b.flows.push(modelgen::Flow {
        id,
        source,
        target,
        condition: None,
    });
    true
}

/// Swap a gateway's kind: a parallel split closed by an exclusive join, or the
/// reverse — the classic "task runs twice" / "join never fires" pair.
fn swap_gateway_kind(b: &mut Builder, rng: &mut Rng) -> bool {
    let gateways: Vec<usize> = b
        .elements
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e.kind, Kind::Parallel | Kind::Exclusive))
        .map(|(i, _)| i)
        .collect();
    if gateways.is_empty() {
        return false;
    }
    let i = gateways[rng.below(gateways.len())];
    b.elements[i].kind = match b.elements[i].kind {
        Kind::Parallel => Kind::Exclusive,
        _ => Kind::Parallel,
    };
    true
}

/// Turn a gateway inclusive — the OR-join this engine refuses outright.
fn make_inclusive(b: &mut Builder, rng: &mut Rng) -> bool {
    let gateways: Vec<usize> = b
        .elements
        .iter()
        .enumerate()
        .filter(|(_, e)| matches!(e.kind, Kind::Parallel | Kind::Exclusive))
        .map(|(i, _)| i)
        .collect();
    if gateways.is_empty() {
        return false;
    }
    b.elements[gateways[rng.below(gateways.len())]].kind = Kind::Inclusive;
    true
}

/// Starve a parallel join by removing one of the flows it waits for.
fn starve_join(b: &mut Builder, rng: &mut Rng) -> bool {
    let joins: Vec<String> = b
        .elements
        .iter()
        .filter(|e| {
            e.kind == Kind::Parallel && b.flows.iter().filter(|f| f.target == e.id).count() > 1
        })
        .map(|e| e.id.clone())
        .collect();
    if joins.is_empty() {
        return false;
    }
    let join = joins[rng.below(joins.len())].clone();
    let incoming: Vec<usize> = b
        .flows
        .iter()
        .enumerate()
        .filter(|(_, f)| f.target == join)
        .map(|(i, _)| i)
        .collect();
    b.flows.remove(incoming[rng.below(incoming.len())]);
    true
}

const MUTATIONS: &[(&str, Mutation)] = &[
    ("retarget_flow", retarget_flow),
    ("resource_flow", resource_flow),
    ("drop_flow", drop_flow),
    ("implicit_split", implicit_split),
    ("swap_gateway_kind", swap_gateway_kind),
    ("make_inclusive", make_inclusive),
    ("starve_join", starve_join),
];

fn any_block() -> impl Strategy<Value = Block> {
    let leaf = Just(Block::Task);
    leaf.prop_recursive(3, 16, 3, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 2..4).prop_map(Block::Seq),
            prop::collection::vec(inner.clone(), 2..4).prop_map(Block::Xor),
            prop::collection::vec(inner.clone(), 2..4).prop_map(Block::Par),
        ]
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// §3c. One structural mutation per valid model; the dichotomy must hold.
    #[test]
    fn mutants_are_rejected_or_harmless(block in any_block(), seed in any::<u64>()) {
        let g = build(&block);
        let mut rng = Rng::new(seed);

        // Try mutations until one applies, so a skipped mutation never
        // silently turns this case into a no-op.
        let mut applied = None;
        for offset in 0..MUTATIONS.len() {
            let (name, mutate) = MUTATIONS[(rng.below(MUTATIONS.len()) + offset) % MUTATIONS.len()];
            let mut skeleton = g.skeleton.clone();
            if mutate(&mut skeleton, &mut rng) {
                let xml = skeleton.to_xml();
                if xml != g.xml {
                    applied = Some((name, xml));
                    break;
                }
            }
        }
        let Some((name, xml)) = applied else {
            return Ok(()); // nothing applicable to this shape
        };

        match judge(&xml) {
            Outcome::Rejected | Outcome::Survived | Outcome::Inconclusive => {}
            Outcome::Hazard(detail) => prop_assert!(
                false,
                "LINTER HOLE via {name}: a lint-clean mutant executes wrongly.\n  \
                 {detail}\n  base block: {:?}\n--- model ---\n{}",
                block,
                xml
            ),
        }
    }
}

/// The mutation fuzz is only worth running if mutants actually get rejected
/// and actually get executed — a run where everything is `Inconclusive` would
/// pass while testing nothing.
#[test]
fn mutation_fuzz_is_not_vacuous() {
    use std::collections::BTreeMap;
    let shapes = [
        Block::Par(vec![Block::Task, Block::Task]),
        Block::Seq(vec![
            Block::Task,
            Block::Xor(vec![Block::Task, Block::Task]),
        ]),
        Block::Par(vec![
            Block::Seq(vec![Block::Task, Block::Task]),
            Block::Xor(vec![Block::Task, Block::Task]),
        ]),
    ];

    let mut tally: BTreeMap<&str, usize> = BTreeMap::new();
    let mut applied_per_mutation: BTreeMap<&str, usize> = BTreeMap::new();
    let mut hazards = Vec::new();

    for (s, block) in shapes.iter().enumerate() {
        let g = build(block);
        for seed in 0..200u64 {
            let mut rng = Rng::new(seed + (s as u64) * 1000);
            let (name, mutate) = MUTATIONS[rng.below(MUTATIONS.len())];
            let mut skeleton = g.skeleton.clone();
            if !mutate(&mut skeleton, &mut rng) {
                continue;
            }
            let xml = skeleton.to_xml();
            if xml == g.xml {
                continue;
            }
            *applied_per_mutation.entry(name).or_default() += 1;
            let outcome = judge(&xml);
            let key = match &outcome {
                Outcome::Rejected => "rejected",
                Outcome::Survived => "survived",
                Outcome::Inconclusive => "inconclusive",
                Outcome::Hazard(d) => {
                    hazards.push(format!("{name}: {d}\n{xml}"));
                    "hazard"
                }
            };
            *tally.entry(key).or_default() += 1;
        }
    }

    println!("mutation outcomes: {tally:?}");
    println!("mutations applied: {applied_per_mutation:?}");
    assert!(
        hazards.is_empty(),
        "LINTER HOLES:\n{}",
        hazards.join("\n---\n")
    );
    assert_eq!(
        applied_per_mutation.len(),
        MUTATIONS.len(),
        "some mutation never applied: {applied_per_mutation:?}"
    );
    let rejected = tally.get("rejected").copied().unwrap_or(0);
    let executed = tally.get("survived").copied().unwrap_or(0);
    assert!(
        rejected > 0,
        "no mutant was ever rejected — mutations are not structural"
    );
    assert!(
        executed > 0,
        "no mutant ever reached execution — the hazard path is untested"
    );
}

// -------------------------------------------------- §3d: the rules earn keep

/// What running a *rejected* model actually does, once the gate is off.
#[derive(Debug, PartialEq)]
enum Consequence {
    /// The semantic core refuses it — `lint should have prevented this`.
    CompileRefused,
    /// It runs, and breaks an invariant: a stuck token, a starved join, a
    /// second token on one join flow. The rule prevents a real hazard.
    Hazard,
    /// It runs cleanly. The rule is conservative rather than hazard-driven —
    /// worth knowing, and not a failure.
    Executes,
}

fn consequence_of(xml: &str) -> (Consequence, String) {
    let defs = rbpmn_model::parse(xml).expect("fixture parses");
    let proc = match ExecutableProcess::compile_without_lint(&defs, "p", &Bindings::default()) {
        Ok(p) => p,
        Err(e) => return (Consequence::CompileRefused, e.to_string()),
    };
    let report = explorer::explore(&proc, json!({}), &[]);
    if !report.violations.is_empty() {
        return (Consequence::Hazard, report.violations.join("; "));
    }
    if report.capped {
        return (Consequence::Hazard, "state space is unbounded".into());
    }
    (
        Consequence::Executes,
        format!("{} states explored cleanly", report.states),
    )
}

/// §3d. For every structural rule, the counterexample it exists to prevent —
/// executed, not asserted. Each row is a claim about *why* a rule is there;
/// if one changes, the table must change with it.
#[test]
fn structural_rules_prevent_a_real_hazard() {
    // Measured, not assumed. Two rows say `Executes`, and that is the honest
    // result: block structure is a *sufficient* condition that makes local
    // join counting provably correct, so individual violations of it need not
    // each manifest a hazard. Those two rules are conservative rather than
    // hazard-driven — worth knowing, and not a reason to relax them.
    let table: &[(&str, &str, Consequence)] = &[
        // fixture, the rule that rejects it, what happens without the gate
        (
            "cross-branch-merge",
            "balanced-gateways",
            Consequence::Hazard,
        ),
        (
            "orphan-parallel-join",
            "balanced-gateways",
            Consequence::Hazard,
        ),
        (
            "two-edges-into-join",
            "balanced-gateways",
            Consequence::Hazard,
        ),
        (
            "end-event-in-branch",
            "balanced-gateways",
            Consequence::Hazard,
        ),
        ("implicit-split", "no-implicit-split", Consequence::Hazard),
        (
            "entry-into-region",
            "balanced-gateways",
            Consequence::Executes,
        ),
        (
            "parallel-missing-join",
            "balanced-gateways",
            Consequence::Executes,
        ),
    ];

    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../rbpmn-model/tests/fixtures/reject");
    let mut wrong = Vec::new();
    for (fixture, rule, expected) in table {
        let xml = fs::read_to_string(dir.join(format!("{fixture}.bpmn")))
            .unwrap_or_else(|e| panic!("{fixture}: {e}"));

        // The rule really is what rejects it.
        let checked = rbpmn_model::check(&xml).expect("fixture parses");
        assert!(!checked.ok, "{fixture} is supposed to be rejected");
        assert!(
            checked
                .diagnostics
                .iter()
                .any(|d| d.rule == *rule && d.severity == rbpmn_model::Severity::Error),
            "{fixture} is not rejected by '{rule}': {:?}",
            checked
                .diagnostics
                .iter()
                .map(|d| &d.rule)
                .collect::<Vec<_>>()
        );

        let (got, detail) = consequence_of(&xml);
        println!("{fixture:<24} [{rule}] -> {got:?}: {detail}");
        if got != *expected {
            wrong.push(format!(
                "{fixture}: expected {expected:?}, got {got:?} ({detail})"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "the recorded consequence of a structural rule changed:\n  {}",
        wrong.join("\n  ")
    );

    // The claim has to have teeth: most of these rules must be backed by a
    // reproducible hazard, not by taste.
    let hazards = table
        .iter()
        .filter(|(_, _, c)| *c == Consequence::Hazard)
        .count();
    assert!(
        hazards >= 5,
        "only {hazards} structural rules demonstrate a hazard — the \
         'restrictions earn their keep' claim no longer holds"
    );
}

/// The headline counterexample, called out on its own because it *is* the
/// Camunda-lineage bug: without `balanced-gateways`, a parallel join collects
/// a second token on one incoming flow and local counting becomes nonsense.
/// The core says so in as many words.
#[test]
fn without_block_structure_a_join_double_counts() {
    let xml = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../rbpmn-model/tests/fixtures/reject/cross-branch-merge.bpmn"),
    )
    .unwrap();
    let (consequence, detail) = consequence_of(&xml);
    assert_eq!(consequence, Consequence::Hazard);
    assert!(
        detail.contains("second token arrived at join")
            && detail.contains("block structure guarantee is broken"),
        "expected the join double-count, got: {detail}"
    );
}
