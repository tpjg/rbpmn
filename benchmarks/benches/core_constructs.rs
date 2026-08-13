//! Pure-core micro-benchmarks: the cost of one `step` transition, per
//! construct.
//!
//! No database, no IO, no clock — `step` is pure, total and deterministic,
//! which is exactly what makes a threshold on these numbers meaningful. This
//! is the layer the regression gate watches (`just bench-micro`), and it is
//! the early-warning system for the semantic core: an accidental O(n²) in
//! gateway handling shows up here as a curve, long before anyone notices a
//! throughput number sag.
//!
//! **Models come from the fixture corpus's own generator**
//! (`crates/rbpmn-core/tests/modelgen`), included rather than copied. It is
//! already the repo's independent second implementation of "what block
//! structure means"; a benchmark that hand-rolled its own block emitter
//! would drift from the corpus, and a construct we support would end up
//! without a cost number.
//!
//! Reading the numbers: a construct's *marginal* cost is the difference
//! against its baseline shape, not its absolute time. `exclusive-split`
//! against `sequence-flow` is one gateway evaluation; `parallel-join/8`
//! against `parallel-join/2` is what widening a join costs.

#[path = "../../crates/rbpmn-core/tests/modelgen/mod.rs"]
mod modelgen;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use modelgen::{Block, build};
use rbpmn_core::{
    Bindings, Command, ExecutableProcess, InstanceState, InstanceStatus, WorkItemId, step,
};
use std::hint::black_box;

fn compile(block: &Block) -> ExecutableProcess {
    let generated = build(block);
    let definitions = rbpmn_model::parse(&generated.xml).expect("the generator emits valid BPMN");
    ExecutableProcess::compile(&definitions, "p", &Bindings::default())
        .expect("the generator emits models this engine compiles")
}

/// A started instance with `completed` of its work items already completed —
/// the setup for measuring the *next* transition rather than the whole run.
fn primed(proc: &ExecutableProcess, completed: usize) -> InstanceState {
    let mut state = InstanceState::new();
    step(
        proc,
        &mut state,
        Command::Start {
            variables: serde_json::json!({}),
        },
    )
    .expect("start");
    for _ in 0..completed {
        let id = first_open(&state).expect("an open work item to complete");
        step(
            proc,
            &mut state,
            Command::CompleteWorkItem {
                id,
                patch: serde_json::json!({}),
            },
        )
        .expect("complete");
    }
    state
}

fn first_open(state: &InstanceState) -> Option<WorkItemId> {
    state.open_work_items().map(|(id, _)| id).next()
}

fn complete_one(proc: &ExecutableProcess, state: &mut InstanceState) {
    let id = first_open(state).expect("an open work item");
    step(
        proc,
        state,
        Command::CompleteWorkItem {
            id,
            patch: serde_json::json!({}),
        },
    )
    .expect("complete");
}

/// One transition each, on the constructs the semantic core is made of.
fn constructs(c: &mut Criterion) {
    let mut group = c.benchmark_group("construct");

    // Baseline: a token crossing a sequence flow into the next task. Every
    // other number in this group is worth reading as a difference from this
    // one.
    let seq = compile(&Block::Seq(vec![Block::Task, Block::Task]));
    group.bench_function("sequence-flow", |b| {
        b.iter_batched_ref(
            || primed(&seq, 0),
            |state| complete_one(&seq, state),
            criterion::BatchSize::SmallInput,
        );
    });

    // The same shape with a gateway in the middle: the difference is one
    // exclusive split, conditions evaluated against the variable document.
    let xor = compile(&Block::Seq(vec![
        Block::Task,
        Block::Xor(vec![Block::Task, Block::Task]),
    ]));
    group.bench_function("exclusive-split", |b| {
        b.iter_batched_ref(
            || primed(&xor, 0),
            |state| complete_one(&xor, state),
            criterion::BatchSize::SmallInput,
        );
    });

    // Entering and leaving an embedded subprocess scope, against the same
    // sequence baseline.
    let scope = compile(&Block::Seq(vec![
        Block::Task,
        Block::Sub(Box::new(Block::Task)),
    ]));
    group.bench_function("scope-enter", |b| {
        b.iter_batched_ref(
            || primed(&scope, 0),
            |state| complete_one(&scope, state),
            criterion::BatchSize::SmallInput,
        );
    });
    group.bench_function("scope-exit", |b| {
        b.iter_batched_ref(
            || primed(&scope, 1),
            |state| complete_one(&scope, state),
            criterion::BatchSize::SmallInput,
        );
    });

    // Width is the axis that matters here: these four points are the shape
    // of the split and join cost curves, and a curve that stops being flat
    // per branch is the bug this whole suite exists to catch.
    for width in [2usize, 4, 8, 16] {
        let par = compile(&Block::Par(vec![Block::Task; width]));

        // The split: `Start` runs from the start event straight into the
        // gateway, so this transition is exactly one split of `width`.
        group.bench_with_input(BenchmarkId::new("parallel-split", width), &width, |b, _| {
            b.iter_batched_ref(
                InstanceState::new,
                |state| {
                    step(
                        &par,
                        state,
                        Command::Start {
                            variables: serde_json::json!({}),
                        },
                    )
                    .expect("start");
                },
                criterion::BatchSize::SmallInput,
            );
        });

        // The join: every branch but one already completed, so the measured
        // transition is the arrival that fires it.
        group.bench_with_input(BenchmarkId::new("parallel-join", width), &width, |b, _| {
            b.iter_batched_ref(
                || primed(&par, width - 1),
                |state| complete_one(&par, state),
                criterion::BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

/// Whole instances, start to terminal state — the pure-core floor under
/// every lifecycle number the persisted benchmarks report.
fn instances(c: &mut Criterion) {
    let mut group = c.benchmark_group("instance");
    let shapes: &[(&str, Block)] = &[
        ("linear-5", Block::Seq(vec![Block::Task; 5])),
        ("parallel-4", Block::Par(vec![Block::Task; 4])),
        (
            "exclusive-chain",
            Block::Seq(vec![
                Block::Xor(vec![Block::Task, Block::Task]),
                Block::Xor(vec![Block::Task, Block::Task]),
                Block::Xor(vec![Block::Task, Block::Task]),
                Block::Xor(vec![Block::Task, Block::Task]),
                Block::Xor(vec![Block::Task, Block::Task]),
            ]),
        ),
        (
            "nested-subprocess",
            Block::Sub(Box::new(Block::Par(vec![
                Block::Task,
                Block::Sub(Box::new(Block::Seq(vec![Block::Task, Block::Task]))),
            ]))),
        ),
    ];
    for (name, block) in shapes {
        let proc = compile(block);
        group.bench_function(*name, |b| {
            b.iter(|| {
                let mut state = InstanceState::new();
                step(
                    &proc,
                    &mut state,
                    Command::Start {
                        variables: serde_json::json!({}),
                    },
                )
                .expect("start");
                while state.status == InstanceStatus::Active {
                    complete_one(&proc, &mut state);
                }
                black_box(state.status)
            });
        });
    }
    group.finish();
}

/// Condition evaluation on its own. Deploy parses conditions once and the
/// compiled form is what `step` evaluates, so these two are separate
/// numbers: only the second one is on the hot path.
fn conditions(c: &mut Criterion) {
    let mut group = c.benchmark_group("condition");
    let source = r#"order.priority = "high" and order.total >= 100 or order.vip = true"#;
    let variables = serde_json::json!({
        "order": { "priority": "high", "total": 250, "vip": false }
    });
    group.bench_function("parse", |b| {
        b.iter(|| rbpmn_model::condition::parse(black_box(source)).expect("parses"));
    });
    let expr = rbpmn_model::condition::parse(source).expect("parses");
    group.bench_function("eval", |b| {
        b.iter(|| rbpmn_model::condition::eval(black_box(&expr), black_box(&variables)));
    });
    group.finish();
}

criterion_group!(benches, constructs, instances, conditions);
criterion_main!(benches);
