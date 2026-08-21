//! The generated-model differential (docs/stress-testing.md §3, items a & b).
//!
//! Two properties over randomly generated block-structured models:
//!
//!   (a) **the linter has no false positives** — every model the generator
//!       emits must lint clean. The generator is an independent second
//!       implementation of "what block structure means", so a disagreement is
//!       either a generator bug or a `balanced-gateways` over-reach, and both
//!       are worth knowing. Nothing else tests this direction: the `reject/`
//!       fixtures only prove the linter catches what it should.
//!
//!   (b) **the structural oracle agrees with the engine** — a small
//!       interpreter over the block tree predicts exactly how often each task
//!       runs; the engine is then driven under the same decisions and must
//!       agree. Two implementations of BPMN semantics, differentially tested.
//!
//! `MsgBoundary` (interrupting message boundary) is in the grammar because a
//! green run after a new phase is not evidence the phase is covered
//! (`docs/stress-testing.md` §3-bis). It makes (b) sharper than any other
//! production: for every activation of a host the oracle must predict *which*
//! of two mutually exclusive paths ran, and counting either both or neither
//! shows up immediately as a multiset that no longer matches. That both paths
//! actually get taken is itself asserted, by
//! `the_message_boundary_production_goes_both_ways`.
//!
//! `SideBoundary` (non-interrupting message boundary) is the same argument one
//! slice later, and it sharpens (b) again in a different direction: the host
//! **always** counts, and the side path counts once *per delivery*, so an
//! oracle that treated the boundary as fire-once — or a core that forgot to
//! re-arm — comes out a multiset short. It is also the only production whose
//! driver checks *state* rather than counts: a delivery must leave the host's
//! work item open, must re-arm under a new subscription id, and must start the
//! side path; completing the host must withdraw the arm, and must not complete
//! the instance while side work is still open
//! (`docs/design/boundary-messages.md` §3.5).
//! `the_side_boundary_production_delivers_none_once_and_twice` is its
//! non-vacuity guard.

mod modelgen;

use modelgen::{
    Block, Decisions, Rng, boundary_hosts, build, decide, expected_executions, run,
    side_boundary_hosts,
};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::TestRunner;
use rbpmn_core::{Bindings, ExecutableProcess, InstanceStatus};
use std::collections::BTreeMap;

/// The grammar minus every boundary — what a side path may hold *inside a
/// scope of its own*. See [`side_body`] for the two things a side path cannot
/// hold directly, and why.
fn plain_block() -> impl Strategy<Value = Block> {
    let leaf = Just(Block::Task);
    leaf.prop_recursive(3, 16, 3, |inner| {
        prop_oneof![
            2 => prop::collection::vec(inner.clone(), 2..4).prop_map(Block::Seq),
            2 => prop::collection::vec(inner.clone(), 2..4).prop_map(Block::Xor),
            2 => prop::collection::vec(inner.clone(), 2..4).prop_map(Block::Par),
            1 => inner.clone().prop_map(|b| Block::Loop(Box::new(b))),
            1 => inner.prop_map(|b| Block::Sub(Box::new(b))),
        ]
    })
}

/// The body of a side path. **A side path is the one multi-token region this
/// grammar can build** — a non-interrupting boundary re-arms, so two side
/// tokens can be walking one path at the same time — and two things inside it
/// are per-scope singletons that two tokens therefore collide on:
///
/// - **a message arm.** Both tokens arm the same `(message, key)`, which is an
///   instance-wide duplicate the core refuses by freezing. That is the engine
///   being right; generating it would only mean generating models that
///   provably cannot complete. So: no boundary anywhere in here.
/// - **a parallel join.** `enter_join` counts arrivals per
///   `(node, scope, arrived_via)`, and both side tokens are in the *host
///   token's* scope — so the second one to arrive on a branch flow trips
///   `second token arrived at join`. Measured on
///   `SideBoundary(Par([Task, Task]))` with two deliveries: 59 of 200 driver
///   interleavings hit it, the rest complete and match the oracle. The model
///   lints completely clean, which makes it an **outcome 1** and not something
///   to generate around silently: it is reported, and
///   `docs/design/boundary-messages.md` §5's "joins inside a side path are
///   ordinary blocks" is the sentence it contradicts. A `Par` is generated
///   here only under a `Sub`, which is the shape that works — a subprocess
///   mints a fresh `ScopeId` per side token, so the joins stop sharing (200 of
///   200 clean). Widen this back to a bare `Par` the day the core or the
///   linter settles it.
///
/// Everything else composes freely and is generated: a loop on a side path
/// really does run twice over, and the driver's loop budget is a count of
/// *completions* precisely so that it still totals what the oracle predicts.
fn side_body() -> impl Strategy<Value = Block> {
    let leaf = Just(Block::Task);
    leaf.prop_recursive(3, 16, 3, |inner| {
        prop_oneof![
            2 => prop::collection::vec(inner.clone(), 2..4).prop_map(Block::Seq),
            2 => prop::collection::vec(inner.clone(), 2..4).prop_map(Block::Xor),
            1 => inner.prop_map(|b| Block::Loop(Box::new(b))),
            2 => plain_block().prop_map(|b| Block::Sub(Box::new(b))),
        ]
    })
}

/// Widths stay small deliberately: cost is exponential in parallel width
/// (docs/stress-testing.md §7), and the interesting structural cases are
/// nesting and composition, not fan-out.
fn any_block() -> impl Strategy<Value = Block> {
    let leaf = Just(Block::Task);
    leaf.prop_recursive(4, 32, 3, |inner| {
        prop_oneof![
            2 => prop::collection::vec(inner.clone(), 2..4).prop_map(Block::Seq),
            2 => prop::collection::vec(inner.clone(), 2..4).prop_map(Block::Xor),
            2 => prop::collection::vec(inner.clone(), 2..4).prop_map(Block::Par),
            1 => inner.clone().prop_map(|b| Block::Loop(Box::new(b))),
            1 => inner.clone().prop_map(|b| Block::Sub(Box::new(b))),
            1 => inner.prop_map(|b| Block::MsgBoundary(Box::new(b))),
            1 => side_body().prop_map(|b| Block::SideBoundary(Box::new(b))),
        ]
    })
}

/// Deeper, wider, bigger. `Par` stays at 6 — §7 measures cost as exponential
/// in parallel width, and this is already past anything a human would model.
fn any_block_wide() -> impl Strategy<Value = Block> {
    let leaf = Just(Block::Task);
    leaf.prop_recursive(6, 96, 5, |inner| {
        prop_oneof![
            2 => prop::collection::vec(inner.clone(), 2..5).prop_map(Block::Seq),
            2 => prop::collection::vec(inner.clone(), 2..5).prop_map(Block::Xor),
            2 => prop::collection::vec(inner.clone(), 2..7).prop_map(Block::Par),
            1 => inner.clone().prop_map(|b| Block::Loop(Box::new(b))),
            1 => inner.clone().prop_map(|b| Block::Sub(Box::new(b))),
            1 => inner.prop_map(|b| Block::MsgBoundary(Box::new(b))),
            1 => side_body().prop_map(|b| Block::SideBoundary(Box::new(b))),
        ]
    })
}

/// The manifest travels with the model: a message boundary's correlation is
/// `Bindings::correlation` on the boundary's own element id, never an
/// attribute in the XML, so compiling a generated model needs the bindings
/// the generator built for it.
fn compile(xml: &str, bindings: &Bindings) -> Result<ExecutableProcess, String> {
    let defs = rbpmn_model::parse(xml).map_err(|e| format!("parse: {e}"))?;
    ExecutableProcess::compile(&defs, "p", bindings).map_err(|e| format!("compile: {e}"))
}

/// Render the failing model so a falsifying case is a `.bpmn` you can paste
/// straight into the playground.
fn report(context: &str, block: &Block, xml: &str, detail: &str) -> String {
    format!("{context}\n  block: {block:?}\n  detail: {detail}\n--- model ---\n{xml}")
}

/// proptest reads `PROPTEST_CASES` only into `ProptestConfig::default()`; a
/// struct literal with `cases:` silently overrides it, which is how the
/// documented `PROPTEST_CASES=20000 cargo test -- --ignored` ran 128 cases
/// for a long time. The environment wins when set; the literal is what a
/// plain `cargo test` runs.
fn cases(default: u32) -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(256), ..ProptestConfig::default() })]

    /// (a) Every generated model lints clean — no errors, and no warnings
    /// either: a warning on a machine-generated, textbook-block-structured
    /// model would mean the rule fires on something it should not.
    #[test]
    fn generated_models_lint_clean(block in any_block()) {
        let g = build(&block);
        let checked = rbpmn_model::check(&g.xml)
            .map_err(|e| report("generated XML did not parse", &block, &g.xml, &e.to_string()))
            .unwrap();
        prop_assert!(
            checked.diagnostics.is_empty(),
            "{}",
            report(
                "generated model is not lint-clean",
                &block,
                &g.xml,
                &checked
                    .diagnostics
                    .iter()
                    .map(|d| format!("[{}] {} on '{}': {}", d.severity, d.rule, d.element, d.message))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        );
    }

    /// (b) The engine executes exactly what the block tree predicts, under
    /// several independent decision assignments and interleavings per model.
    #[test]
    fn engine_matches_the_structural_oracle(block in any_block(), seed in any::<u64>()) {
        let g = build(&block);
        let proc = compile(&g.xml, &g.bindings)
            .map_err(|e| report("generated model did not compile", &block, &g.xml, &e))
            .unwrap();

        for round in 0..4u64 {
            let mut rng = Rng::new(seed ^ (round.wrapping_mul(0x9E37_79B9_7F4A_7C15)));
            let decisions = decide(&g.root, &mut rng, 3);
            let expected = expected_executions(&g.root, &decisions);

            let actual = run(&proc, &g.root, &decisions, &mut rng, 10_000)
                .map_err(|e| report("driving the engine failed", &block, &g.xml, &e))
                .unwrap();

            prop_assert_eq!(
                actual.status,
                InstanceStatus::Completed,
                "{}",
                report("instance did not complete", &block, &g.xml, &format!("{:?}", actual.status))
            );
            // Every driver step is exactly one unit of work — a work item
            // completed, or a message delivered to a boundary — so the step
            // count must equal the total executions: a guard against a run
            // that silently did nothing.
            prop_assert_eq!(actual.steps, expected.values().sum::<usize>());
            prop_assert_eq!(
                &actual.executions,
                &expected,
                "{}",
                report(
                    "engine and oracle disagree",
                    &block,
                    &g.xml,
                    &format!("decisions: {decisions:?}")
                )
            );
        }
    }
}

/// Hand-written shapes the random generator reaches only rarely, kept as a
/// fast, always-run smoke test of both properties.
#[test]
fn known_shapes_lint_clean_and_match_the_oracle() {
    let shapes = vec![
        ("single task", Block::Task),
        ("sequence", Block::Seq(vec![Block::Task, Block::Task])),
        ("parallel", Block::Par(vec![Block::Task, Block::Task])),
        ("exclusive", Block::Xor(vec![Block::Task, Block::Task])),
        ("loop", Block::Loop(Box::new(Block::Task))),
        (
            "xor inside par",
            Block::Par(vec![
                Block::Xor(vec![Block::Task, Block::Task]),
                Block::Task,
            ]),
        ),
        (
            "par inside xor",
            Block::Xor(vec![
                Block::Par(vec![Block::Task, Block::Task]),
                Block::Task,
            ]),
        ),
        (
            "loop around a parallel block",
            Block::Loop(Box::new(Block::Par(vec![Block::Task, Block::Task]))),
        ),
        (
            "nested loops",
            Block::Loop(Box::new(Block::Seq(vec![
                Block::Loop(Box::new(Block::Task)),
                Block::Task,
            ]))),
        ),
        (
            "loop inside a parallel branch",
            Block::Par(vec![Block::Loop(Box::new(Block::Task)), Block::Task]),
        ),
        ("subprocess", Block::Sub(Box::new(Block::Task))),
        (
            "subprocess wrapping a parallel block",
            Block::Sub(Box::new(Block::Par(vec![Block::Task, Block::Task]))),
        ),
        (
            "nested subprocesses",
            Block::Sub(Box::new(Block::Seq(vec![
                Block::Sub(Box::new(Block::Task)),
                Block::Task,
            ]))),
        ),
        (
            "subprocess inside a parallel branch",
            Block::Par(vec![Block::Sub(Box::new(Block::Task)), Block::Task]),
        ),
        (
            "loop around a subprocess",
            Block::Loop(Box::new(Block::Sub(Box::new(Block::Task)))),
        ),
        (
            "subprocess wrapping a loop",
            Block::Sub(Box::new(Block::Loop(Box::new(Block::Task)))),
        ),
        (
            "three-way parallel of sequences",
            Block::Par(vec![
                Block::Seq(vec![Block::Task, Block::Task]),
                Block::Seq(vec![Block::Task, Block::Task]),
                Block::Task,
            ]),
        ),
        // The message boundary in every position the grammar composes it
        // into: the arm/withdraw interplay with joins, loops and scope
        // teardown is exactly what a hand-written fixture cannot enumerate.
        (
            "message boundary",
            Block::MsgBoundary(Box::new(Block::Task)),
        ),
        (
            "message boundary in a sequence",
            Block::Seq(vec![
                Block::Task,
                Block::MsgBoundary(Box::new(Block::Task)),
                Block::Task,
            ]),
        ),
        (
            "message boundary inside a parallel branch",
            Block::Par(vec![Block::MsgBoundary(Box::new(Block::Task)), Block::Task]),
        ),
        (
            "message boundary inside an exclusive branch",
            Block::Xor(vec![Block::MsgBoundary(Box::new(Block::Task)), Block::Task]),
        ),
        // Two arms open at once, in sibling branches. They stay legal because
        // each boundary catches a message of its own — a shared one would be
        // a duplicate `(message, key)` and freeze the instance.
        (
            "two message boundaries armed concurrently",
            Block::Par(vec![
                Block::MsgBoundary(Box::new(Block::Task)),
                Block::MsgBoundary(Box::new(Block::Task)),
            ]),
        ),
        // A fresh subscription per iteration: the previous one is withdrawn
        // when the host leaves, so the duplicate rule must not trip.
        (
            "loop around a message boundary",
            Block::Loop(Box::new(Block::MsgBoundary(Box::new(Block::Task)))),
        ),
        (
            "loop around a message boundary inside a parallel branch",
            Block::Par(vec![
                Block::Loop(Box::new(Block::MsgBoundary(Box::new(Block::Task)))),
                Block::Task,
            ]),
        ),
        (
            "subprocess wrapping a message boundary",
            Block::Sub(Box::new(Block::MsgBoundary(Box::new(Block::Task)))),
        ),
        (
            "message boundary handling a parallel block",
            Block::MsgBoundary(Box::new(Block::Par(vec![Block::Task, Block::Task]))),
        ),
        (
            "message boundary handling a loop",
            Block::MsgBoundary(Box::new(Block::Loop(Box::new(Block::Task)))),
        ),
        (
            "message boundary handling a subprocess",
            Block::MsgBoundary(Box::new(Block::Sub(Box::new(Block::Task)))),
        ),
        (
            "message boundary on the handler of a message boundary",
            Block::MsgBoundary(Box::new(Block::MsgBoundary(Box::new(Block::Task)))),
        ),
        // The non-interrupting boundary, in every position that composes. The
        // difference from the block above is not the position but the
        // *arity*: one activation can spawn two side tokens, so several of
        // these run two concurrent copies of the same side path.
        (
            "non-interrupting message boundary",
            Block::SideBoundary(Box::new(Block::Task)),
        ),
        (
            "non-interrupting boundary in a sequence",
            Block::Seq(vec![
                Block::Task,
                Block::SideBoundary(Box::new(Block::Task)),
                Block::Task,
            ]),
        ),
        // Fixture 37's shape, generated: legal only because the region
        // analysis ignores non-interrupting pseudo-edges, so the side path's
        // own end event is not "a plain end inside the region" and the branch
        // still delivers exactly one token to the join.
        (
            "non-interrupting boundary inside a parallel branch",
            Block::Par(vec![
                Block::SideBoundary(Box::new(Block::Task)),
                Block::Task,
            ]),
        ),
        (
            "non-interrupting boundary inside an exclusive branch",
            Block::Xor(vec![
                Block::SideBoundary(Box::new(Block::Task)),
                Block::Task,
            ]),
        ),
        (
            "two non-interrupting boundaries armed concurrently",
            Block::Par(vec![
                Block::SideBoundary(Box::new(Block::Task)),
                Block::SideBoundary(Box::new(Block::Task)),
            ]),
        ),
        // A fresh arm per iteration, withdrawn when the host completes — and
        // side tokens from the previous iteration may still be running while
        // the next one arms.
        (
            "loop around a non-interrupting boundary",
            Block::Loop(Box::new(Block::SideBoundary(Box::new(Block::Task)))),
        ),
        // The side path's end event lives in the child scope: the scope
        // completes when its last token is consumed, side tokens included.
        (
            "subprocess wrapping a non-interrupting boundary",
            Block::Sub(Box::new(Block::SideBoundary(Box::new(Block::Task)))),
        ),
        // A parallel block on a side path, wrapped in a subprocess — the
        // only form of it that is sound, and the reason is in `side_body`:
        // the scope instance is what stops two side tokens sharing one join.
        (
            "side path whose subprocess holds a parallel block",
            Block::SideBoundary(Box::new(Block::Sub(Box::new(Block::Par(vec![
                Block::Task,
                Block::Task,
            ]))))),
        ),
        // Two side tokens going round one loop at once — the case the
        // driver's loop budget has to be a count of completions to survive.
        (
            "side path that is a loop",
            Block::SideBoundary(Box::new(Block::Loop(Box::new(Block::Task)))),
        ),
        (
            "side path that is a subprocess",
            Block::SideBoundary(Box::new(Block::Sub(Box::new(Block::Task)))),
        ),
        // Nothing between the boundary and its end event: the side token is
        // spawned and consumed inside one step, which is the only shape where
        // "a delivery started the side path" cannot be checked by looking for
        // a new work item. The empty `Seq` is safe *here* — directly under a
        // boundary, where the entry flow carries no condition to drop.
        (
            "side path that is only an end event",
            Block::SideBoundary(Box::new(Block::Seq(vec![]))),
        ),
        (
            "non-interrupting boundary on the handler of an interrupting one",
            Block::MsgBoundary(Box::new(Block::SideBoundary(Box::new(Block::Task)))),
        ),
    ];

    for (name, block) in shapes {
        let g = build(&block);
        let checked = rbpmn_model::check(&g.xml).unwrap_or_else(|e| panic!("{name}: parse: {e}"));
        assert!(
            checked.diagnostics.is_empty(),
            "{}",
            report(
                &format!("{name}: not lint-clean"),
                &block,
                &g.xml,
                &checked
                    .diagnostics
                    .iter()
                    .map(|d| format!(
                        "[{}] {} on '{}': {}",
                        d.severity, d.rule, d.element, d.message
                    ))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        );

        let proc = compile(&g.xml, &g.bindings).unwrap_or_else(|e| panic!("{name}: {e}"));
        for seed in 0..8u64 {
            let mut rng = Rng::new(seed);
            let decisions = decide(&g.root, &mut rng, 3);
            let expected = expected_executions(&g.root, &decisions);
            let actual = run(&proc, &g.root, &decisions, &mut rng, 10_000)
                .unwrap_or_else(|e| panic!("{name} (seed {seed}): {e}"));
            assert_eq!(
                actual.status,
                InstanceStatus::Completed,
                "{name} (seed {seed})"
            );
            assert_eq!(
                actual.steps,
                expected.values().sum::<usize>(),
                "{name} (seed {seed})"
            );
            assert_eq!(
                actual.executions, expected,
                "{name} (seed {seed}): engine and oracle disagree, decisions {decisions:?}\n{}",
                g.xml
            );
        }
    }
}

/// The oracle must be worth differentialling against: check it against
/// hand-computed answers, so a bug in *it* cannot silently excuse the engine.
#[test]
fn the_oracle_itself_is_right() {
    /// (model, decisions, hand-computed expected executions)
    type Case = (Block, Decisions, Vec<(&'static str, usize)>);
    let cases: Vec<Case> = vec![
        // A sequence runs each task once.
        (
            Block::Seq(vec![Block::Task, Block::Task]),
            Decisions::default(),
            vec![("t1", 1), ("t2", 1)],
        ),
        // Parallel runs every branch.
        (
            Block::Par(vec![Block::Task, Block::Task]),
            Decisions::default(),
            vec![("t1", 1), ("t2", 1)],
        ),
        // Exclusive runs exactly the chosen branch.
        (
            Block::Xor(vec![Block::Task, Block::Task]),
            Decisions {
                xor: [("x1".to_string(), 0)].into(),
                loops: Default::default(),
                deliver: Default::default(),
                side: Default::default(),
            },
            vec![("t1", 1)],
        ),
        (
            Block::Xor(vec![Block::Task, Block::Task]),
            Decisions {
                xor: [("x1".to_string(), 1)].into(),
                loops: Default::default(),
                deliver: Default::default(),
                side: Default::default(),
            },
            vec![("t2", 1)],
        ),
        // A loop runs its body and its control task n times each.
        (
            Block::Loop(Box::new(Block::Task)),
            Decisions {
                xor: Default::default(),
                loops: [("l1".to_string(), 3)].into(),
                deliver: Default::default(),
                side: Default::default(),
            },
            vec![("t2", 3), ("lctl1", 3)],
        ),
        // An unscheduled message boundary completes its host, and the
        // handler never runs.
        (
            Block::MsgBoundary(Box::new(Block::Task)),
            Decisions::default(),
            vec![("t1", 1)],
        ),
        // A delivered one runs the boundary and its handler instead — and the
        // host counts *not at all*: it started, and was cancelled.
        (
            Block::MsgBoundary(Box::new(Block::Task)),
            Decisions {
                xor: Default::default(),
                loops: Default::default(),
                deliver: [("b1".to_string(), vec![true])].into(),
                side: Default::default(),
            },
            vec![("b1", 1), ("t2", 1)],
        ),
        // Per activation, not per boundary: the first pass is interrupted,
        // the second completes, and the control task closes both.
        (
            Block::Loop(Box::new(Block::MsgBoundary(Box::new(Block::Task)))),
            Decisions {
                xor: Default::default(),
                loops: [("l1".to_string(), 2)].into(),
                deliver: [("b1".to_string(), vec![true, false])].into(),
                side: Default::default(),
            },
            vec![("b1", 1), ("t3", 1), ("t2", 1), ("lctl1", 2)],
        ),
        // Non-interrupting, nothing delivered: the host runs, and that is all
        // that ever happens. The mirror of the interrupting case above, where
        // "nothing delivered" also means "the host runs" — the two only part
        // company once a message arrives.
        (
            Block::SideBoundary(Box::new(Block::Task)),
            Decisions::default(),
            vec![("t1", 1)],
        ),
        // One delivery: the host *still* counts — it was never touched — and
        // the boundary and its path count beside it. An oracle that reused
        // the interrupting rule would drop `t1` here.
        (
            Block::SideBoundary(Box::new(Block::Task)),
            Decisions {
                xor: Default::default(),
                loops: Default::default(),
                deliver: Default::default(),
                side: [("b1".to_string(), vec![1])].into(),
            },
            vec![("t1", 1), ("b1", 1), ("t2", 1)],
        ),
        // Two: the boundary re-armed, so the path runs twice and the host
        // still once. This is the count that separates "it re-arms" from "it
        // fired, that was that".
        (
            Block::SideBoundary(Box::new(Block::Task)),
            Decisions {
                xor: Default::default(),
                loops: Default::default(),
                deliver: Default::default(),
                side: [("b1".to_string(), vec![2])].into(),
            },
            vec![("t1", 1), ("b1", 2), ("t2", 2)],
        ),
        // Per activation, not per boundary: a loop around the host reads the
        // next schedule entry each iteration — two deliveries on the first
        // pass, none on the second, the host and the control task on both.
        (
            Block::Loop(Box::new(Block::SideBoundary(Box::new(Block::Task)))),
            Decisions {
                xor: Default::default(),
                loops: [("l1".to_string(), 2)].into(),
                deliver: Default::default(),
                side: [("b1".to_string(), vec![2, 0])].into(),
            },
            vec![("t2", 2), ("b1", 2), ("t3", 2), ("lctl1", 2)],
        ),
    ];

    for (block, decisions, want) in cases {
        let g = build(&block);
        let got = expected_executions(&g.root, &decisions);
        let want: std::collections::BTreeMap<String, usize> =
            want.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
        assert_eq!(got, want, "oracle wrong for {block:?} under {decisions:?}");
    }
}

/// What one deterministic sweep over a fixed set of generated models saw.
///
/// The sweep itself is the same work `engine_matches_the_structural_oracle`
/// does — lint, compile, drive, compare — so it proves nothing new on its own.
/// Its point is the tally: everything above stays green on a grammar that
/// never emits a boundary, on a driver that never delivers, and on a schedule
/// that happens to say "complete" every time. Each of those is a silent hole,
/// and the two tests below are the only things that would notice. This is the
/// storm's "never went both ways" applied to the generator
/// (`docs/stress-testing.md` §3-bis).
#[derive(Default)]
struct Sweep {
    models: usize,
    runs: usize,
    /// Interrupting message boundaries: models that carried one, how many, and
    /// how often each exit of the race was taken.
    with_boundary: usize,
    boundaries: usize,
    delivered: usize,
    hosts_completed: usize,
    /// Non-interrupting boundaries: models that carried one, how many, and the
    /// per-activation delivery histogram — a total would be satisfied by a
    /// sweep that always delivered exactly once.
    with_side: usize,
    side_boundaries: usize,
    side_histogram: BTreeMap<usize, usize>,
    side_with_open_work: usize,
}

/// Deterministic on purpose — `TestRunner::deterministic()` and the seeded
/// `Rng` mean the same models, the same schedules and the same traces on every
/// run, so a failure reproduces forever.
fn sweep(models: usize, rounds: u64) -> Sweep {
    let strategy = any_block();
    let mut runner = TestRunner::deterministic();
    let mut sw = Sweep {
        models,
        runs: models * rounds as usize,
        ..Sweep::default()
    };

    for i in 0..models {
        let block = strategy
            .new_tree(&mut runner)
            .expect("the block strategy always produces a value")
            .current();
        let g = build(&block);

        let checked =
            rbpmn_model::check(&g.xml).unwrap_or_else(|e| panic!("model {i}: parse: {e}"));
        assert!(
            checked.diagnostics.is_empty(),
            "{}",
            report(
                &format!("model {i}: not lint-clean"),
                &block,
                &g.xml,
                &checked
                    .diagnostics
                    .iter()
                    .map(|d| format!(
                        "[{}] {} on '{}': {}",
                        d.severity, d.rule, d.element, d.message
                    ))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        );

        let hosts = boundary_hosts(&g.root);
        if !hosts.is_empty() {
            sw.with_boundary += 1;
            sw.boundaries += hosts.len();
        }
        let side = side_boundary_hosts(&g.root);
        if !side.is_empty() {
            sw.with_side += 1;
            sw.side_boundaries += side.len();
        }

        let proc = compile(&g.xml, &g.bindings).unwrap_or_else(|e| panic!("model {i}: {e}"));
        for round in 0..rounds {
            let mut rng = Rng::new((i as u64).wrapping_mul(rounds).wrapping_add(round));
            let decisions = decide(&g.root, &mut rng, 3);
            let expected = expected_executions(&g.root, &decisions);
            let actual = run(&proc, &g.root, &decisions, &mut rng, 10_000)
                .unwrap_or_else(|e| panic!("{}", report("driving failed", &block, &g.xml, &e)));

            assert_eq!(actual.status, InstanceStatus::Completed, "model {i}");
            assert_eq!(actual.steps, expected.values().sum::<usize>(), "model {i}");
            assert_eq!(
                actual.executions,
                expected,
                "{}",
                report(
                    &format!("model {i}: engine and oracle disagree"),
                    &block,
                    &g.xml,
                    &format!("decisions: {decisions:?}")
                )
            );
            sw.delivered += actual.delivered;
            sw.hosts_completed += actual.hosts_completed;
            for n in actual.side_deliveries {
                *sw.side_histogram.entry(n).or_default() += 1;
            }
            sw.side_with_open_work += actual.hosts_completed_with_side_work;
        }
    }
    sw
}

/// **Non-vacuity for the interrupting message boundary.** The production must
/// be reached, and both of its exits taken.
#[test]
fn the_message_boundary_production_goes_both_ways() {
    let sw = sweep(200, 4);
    println!(
        "{}/{} generated models carried a message boundary ({} boundaries in all); \
         across {} runs the message was delivered {} times and the host completed \
         {} times",
        sw.with_boundary, sw.models, sw.boundaries, sw.runs, sw.delivered, sw.hosts_completed
    );
    assert!(
        sw.with_boundary > 0,
        "no generated model contained a message boundary — the production is \
         unreachable in the random walk, so every property above passed \
         without ever seeing one"
    );
    assert!(
        sw.delivered > 0,
        "the message was never delivered in {} runs — the completed path is \
         the only one under test",
        sw.runs
    );
    assert!(
        sw.hosts_completed > 0,
        "no message-boundary host was ever completed in {} runs — the \
         interrupted path is the only one under test",
        sw.runs
    );
}

/// **Non-vacuity for the non-interrupting message boundary.** Reaching the
/// production is not enough here, and neither is a delivery count: the
/// schedule offers 0, 1 or 2 messages per activation, and each of the three
/// tests something the others do not.
///
/// - **zero** is the only case where the boundary arms, never fires, and is
///   withdrawn — the whole of it is the withdrawal the driver checks;
/// - **one** is a side token spawned beside a live host;
/// - **two** is the *re-arm*, and nothing else here proves it happened. A core
///   that armed once and forgot would satisfy every count below except this
///   one, and the driver's "no armed subscription at ..." error would be the
///   first thing to fire.
///
/// The last assertion is `docs/design/boundary-messages.md` §3.5: a host that
/// completed while its side path still had work open must not take the
/// instance with it. The driver checks that at every such completion; this
/// insists such a completion actually occurred.
#[test]
fn the_side_boundary_production_delivers_none_once_and_twice() {
    let sw = sweep(200, 4);
    println!(
        "{}/{} generated models carried a non-interrupting boundary ({} of them in \
         all); across {} runs its {} activations were delivered to {:?} (deliveries \
         -> activations), and {} hosts completed with side work still open",
        sw.with_side,
        sw.models,
        sw.side_boundaries,
        sw.runs,
        sw.side_histogram.values().sum::<usize>(),
        sw.side_histogram,
        sw.side_with_open_work
    );
    assert!(
        sw.with_side > 0,
        "no generated model contained a non-interrupting boundary — the production \
         is unreachable in the random walk, so every property above passed without \
         ever seeing one"
    );
    for n in 0..=modelgen::MAX_SIDE_DELIVERIES {
        assert!(
            sw.side_histogram.contains_key(&n),
            "no activation of a non-interrupting boundary ever received {n} \
             message(s) in {} runs: {:?}. The schedule offers 0..={}, so a gap \
             means that many models were driven down one path only{}",
            sw.runs,
            sw.side_histogram,
            modelgen::MAX_SIDE_DELIVERIES,
            if n == modelgen::MAX_SIDE_DELIVERIES {
                " — and this is the end that proves the boundary re-armed"
            } else {
                ""
            }
        );
    }
    assert!(
        sw.side_with_open_work > 0,
        "no side-boundary host ever completed while its side path still had work \
         open in {} runs — the claim that a side token keeps the scope alive \
         (design §3.5) was never put to the test",
        sw.runs
    );
}

proptest! {
    #![proptest_config(ProptestConfig { cases: cases(128), ..ProptestConfig::default() })]

    /// The wide sweep — deeper nesting and parallel widths up to 6. Run it
    /// with volume when hunting: `PROPTEST_CASES=20000 cargo test -- --ignored`.
    #[test]
    #[ignore = "wide sweep: run explicitly with --ignored"]
    fn wide_models_lint_clean_and_match_the_oracle(block in any_block_wide(), seed in any::<u64>()) {
        let g = build(&block);
        let checked = rbpmn_model::check(&g.xml)
            .map_err(|e| report("generated XML did not parse", &block, &g.xml, &e.to_string()))
            .unwrap();
        prop_assert!(
            checked.diagnostics.is_empty(),
            "{}",
            report(
                "wide model is not lint-clean",
                &block,
                &g.xml,
                &checked
                    .diagnostics
                    .iter()
                    .map(|d| format!("[{}] {} on '{}': {}", d.severity, d.rule, d.element, d.message))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        );

        let proc = compile(&g.xml, &g.bindings)
            .map_err(|e| report("wide model did not compile", &block, &g.xml, &e))
            .unwrap();
        let mut rng = Rng::new(seed);
        let decisions = decide(&g.root, &mut rng, 3);
        let expected = expected_executions(&g.root, &decisions);
        let actual = run(&proc, &g.root, &decisions, &mut rng, 100_000)
            .map_err(|e| report("driving the engine failed", &block, &g.xml, &e))
            .unwrap();
        prop_assert_eq!(actual.status, InstanceStatus::Completed);
        prop_assert_eq!(&actual.executions, &expected, "{}",
            report("engine and oracle disagree", &block, &g.xml, &format!("{decisions:?}")));
    }
}
