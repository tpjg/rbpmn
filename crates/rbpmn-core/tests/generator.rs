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

mod modelgen;

use modelgen::{Block, Decisions, Rng, build, decide, expected_executions, run};
use proptest::prelude::*;
use rbpmn_core::{Bindings, ExecutableProcess, InstanceStatus};

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
            1 => inner.prop_map(|b| Block::Loop(Box::new(b))),
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
            1 => inner.prop_map(|b| Block::Loop(Box::new(b))),
        ]
    })
}

fn compile(xml: &str) -> Result<ExecutableProcess, String> {
    let defs = rbpmn_model::parse(xml).map_err(|e| format!("parse: {e}"))?;
    ExecutableProcess::compile(&defs, "p", &Bindings::default())
        .map_err(|e| format!("compile: {e}"))
}

/// Render the failing model so a falsifying case is a `.bpmn` you can paste
/// straight into the playground.
fn report(context: &str, block: &Block, xml: &str, detail: &str) -> String {
    format!("{context}\n  block: {block:?}\n  detail: {detail}\n--- model ---\n{xml}")
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

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
        let proc = compile(&g.xml)
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
            // Every driver step completes exactly one work item, so the step
            // count must equal the total executions — a guard against a run
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
        (
            "three-way parallel of sequences",
            Block::Par(vec![
                Block::Seq(vec![Block::Task, Block::Task]),
                Block::Seq(vec![Block::Task, Block::Task]),
                Block::Task,
            ]),
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

        let proc = compile(&g.xml).unwrap_or_else(|e| panic!("{name}: {e}"));
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
            },
            vec![("t1", 1)],
        ),
        (
            Block::Xor(vec![Block::Task, Block::Task]),
            Decisions {
                xor: [("x1".to_string(), 1)].into(),
                loops: Default::default(),
            },
            vec![("t2", 1)],
        ),
        // A loop runs its body and its control task n times each.
        (
            Block::Loop(Box::new(Block::Task)),
            Decisions {
                xor: Default::default(),
                loops: [("l1".to_string(), 3)].into(),
            },
            vec![("t2", 3), ("lctl1", 3)],
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

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

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

        let proc = compile(&g.xml)
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
