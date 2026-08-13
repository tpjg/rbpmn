//! The persisted half of the pattern micro-benchmarks: what a construct
//! costs once the rows it causes are actually written.
//!
//! **Reported, never gated.** The pure-core suite is deterministic enough
//! for a threshold; this one shares a machine with a database, a
//! checkpointer and an autovacuum daemon, and a CI gate on it would be a
//! coin toss with a build attached.
//!
//! Same models as the pure-core suite — the fixture corpus's own generator
//! — and the same reading: a construct's cost is the difference against the
//! `single-task` baseline, which is one instance's unavoidable start,
//! work-item and completion traffic.
//!
//! Deliberately sequential: one instance at a time, one connection. This
//! measures the *latency* a construct adds, not how many of them a machine
//! can do at once. Concurrency is the lifecycle benchmark's job, and mixing
//! the two produces a number that answers neither question.

#[path = "../../crates/rbpmn-core/tests/modelgen/mod.rs"]
mod modelgen;

use crate::env::{Checkout, Hardware};
use crate::pg;
use modelgen::{Block, build};
use rbpmn_engine::Engine;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const SCHEMA: &str = "rbpmn-bench-persisted/1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedReport {
    pub schema: String,
    pub started_at: String,
    pub iterations: u32,
    pub checkout: Checkout,
    pub hardware: Hardware,
    pub postgres: pg::PostgresFacts,
    pub constructs: Vec<ConstructCost>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConstructCost {
    pub name: String,
    /// Steps per instance: one `Start` plus one completion per work item.
    pub steps_per_instance: f64,
    pub micros_per_instance: f64,
    pub micros_per_step: f64,
    /// Against `single-task`. Zero for the baseline itself.
    pub marginal_micros_vs_baseline: f64,
    pub events_per_instance: f64,
    /// Total bytes across every `rbpmn_` relation, divided by instances —
    /// the storage each instance of this shape leaves behind.
    pub bytes_per_instance: f64,
}

/// The shapes, in the order the report prints them. `single-task` is first
/// because every other row is read against it.
fn shapes() -> Vec<(&'static str, Block)> {
    vec![
        ("single-task", Block::Task),
        ("sequence-2", Block::Seq(vec![Block::Task, Block::Task])),
        (
            "exclusive-2",
            Block::Seq(vec![
                Block::Task,
                Block::Xor(vec![Block::Task, Block::Task]),
            ]),
        ),
        ("parallel-2", Block::Par(vec![Block::Task; 2])),
        ("parallel-4", Block::Par(vec![Block::Task; 4])),
        ("parallel-8", Block::Par(vec![Block::Task; 8])),
        (
            "subprocess",
            Block::Seq(vec![Block::Task, Block::Sub(Box::new(Block::Task))]),
        ),
        ("loop-3", Block::Loop(Box::new(Block::Task))),
    ]
}

pub async fn run(
    root: &Path,
    database_url: &str,
    provisioned_by: &str,
    iterations: u32,
) -> Result<PersistedReport, String> {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(30))
        .connect(database_url)
        .await
        .map_err(|e| format!("connecting to {}: {e}", crate::run::redact(database_url)))?;
    let engine = Engine::builder(pool.clone()).build();
    engine
        .migrate()
        .await
        .map_err(|e| format!("migrate: {e}"))?;

    let facts = pg::facts(&pool, &crate::run::host_of(database_url), provisioned_by)
        .await
        .map_err(|e| format!("capturing postgres settings: {e}"))?;

    let mut constructs = Vec::new();
    let mut baseline_micros = 0.0f64;
    for (name, block) in shapes() {
        let cost = measure(&engine, &pool, name, &block, iterations).await?;
        if name == "single-task" {
            baseline_micros = cost.micros_per_instance;
        }
        constructs.push(ConstructCost {
            marginal_micros_vs_baseline: cost.micros_per_instance - baseline_micros,
            ..cost
        });
    }

    let hardware = Hardware::detect(root);
    let checkout = Checkout::detect(root);
    let mut warnings = hardware.declaration_mismatches.clone();
    warnings.push(
        "persisted micro-benchmarks are reported, never gated: they share a machine with \
         a database, a checkpointer and autovacuum. Only the pure-core suite \
         (`just bench-micro`) is deterministic enough to fail a build."
            .to_string(),
    );
    if checkout.dirty {
        warnings.push("the checkout has uncommitted changes".to_string());
    }

    Ok(PersistedReport {
        schema: SCHEMA.to_string(),
        started_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        iterations,
        checkout,
        hardware,
        postgres: facts,
        constructs,
        warnings,
    })
}

async fn measure(
    engine: &Engine,
    pool: &PgPool,
    name: &str,
    block: &Block,
    iterations: u32,
) -> Result<ConstructCost, String> {
    let generated = build(block);
    let xml = with_process_id(&generated.xml, name);
    engine
        .deploy(&xml, &rbpmn_core::Bindings::default())
        .await
        .map_err(|e| format!("deploying the {name} shape: {e}"))?;

    // The loop shape's back-edge is decided by a variable the control task
    // writes; without one the default flow exits after a single pass, which
    // is a loop that never loops. Three passes is enough to see the cost of
    // re-entry without turning the benchmark into a loop benchmark.
    let looping = matches!(block, Block::Loop(_));

    // A short warmup so the plan cache, the pool and the compiled-definition
    // cache are all warm before the clock starts.
    for _ in 0..(iterations / 10).max(1) {
        drive_one(engine, pool, name, looping).await?;
    }
    // *After* the warmup: the footprint is divided by the measured
    // iterations, so counting the warmup's rows inflated every
    // events-per-instance figure by a tenth.
    let before = footprint(pool).await?;
    let started = Instant::now();
    let mut steps = 0u64;
    for _ in 0..iterations {
        steps += drive_one(engine, pool, name, looping).await?;
    }
    let elapsed = started.elapsed();
    let after = footprint(pool).await?;

    let iterations = iterations as f64;
    Ok(ConstructCost {
        name: name.to_string(),
        steps_per_instance: steps as f64 / iterations,
        micros_per_instance: elapsed.as_secs_f64() * 1e6 / iterations,
        micros_per_step: elapsed.as_secs_f64() * 1e6 / steps.max(1) as f64,
        marginal_micros_vs_baseline: 0.0,
        events_per_instance: (after.events - before.events) as f64 / iterations,
        bytes_per_instance: (after.bytes - before.bytes) as f64 / iterations,
    })
}

/// Start one instance and complete every work item it offers until it is
/// terminal. Returns the number of steps that took (the `Start` plus one per
/// completion), so per-step cost is derived rather than assumed.
async fn drive_one(
    engine: &Engine,
    pool: &PgPool,
    key: &str,
    looping: bool,
) -> Result<u64, String> {
    let started = engine
        .start(key, None, serde_json::json!({}))
        .await
        .map_err(|e| format!("start {key}: {e}"))?;
    let mut steps = 1u64;
    let mut passes = 0u32;
    loop {
        let open: Vec<uuid::Uuid> = sqlx::query_scalar(
            "select id from rbpmn_work_item where instance_id = $1 \
             and state in ('available','locked') order by item_no",
        )
        .bind(started.id)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("listing open work items: {e}"))?;
        if open.is_empty() {
            return Ok(steps);
        }
        for id in open {
            // The loop shape's control task decides re-entry; everything
            // else ignores the patch.
            let patch = if looping {
                passes += 1;
                serde_json::json!({ "l1": passes < 3 })
            } else {
                serde_json::json!({})
            };
            engine
                .complete_work_item(id, patch)
                .await
                .map_err(|e| format!("completing a {key} work item: {e}"))?;
            steps += 1;
        }
        if steps > 1_000 {
            return Err(format!(
                "the {key} shape did not terminate in 1000 steps — a loop that never exits"
            ));
        }
    }
}

struct Footprint {
    events: i64,
    bytes: i64,
}

async fn footprint(pool: &PgPool) -> Result<Footprint, String> {
    let row = sqlx::query(
        "select (select count(*) from rbpmn_event) as events, \
                coalesce(sum(pg_total_relation_size(relid)), 0)::bigint as bytes \
         from pg_stat_user_tables where relname like 'rbpmn\\_%'",
    )
    .fetch_one(pool)
    .await
    .map_err(|e| format!("measuring the storage footprint: {e}"))?;
    Ok(Footprint {
        events: row.get("events"),
        bytes: row.get("bytes"),
    })
}

/// The generator always emits process id `p`; each shape needs its own
/// definition key or they become successive versions of one definition and
/// `start` silently runs the wrong model.
fn with_process_id(xml: &str, id: &str) -> String {
    xml.replace(
        "<bpmn:process id=\"p\"",
        &format!("<bpmn:process id=\"{id}\""),
    )
}

impl PersistedReport {
    pub fn write(&self, root: &Path) -> Result<PathBuf, String> {
        let date = self.started_at.get(..10).unwrap_or("unknown-date");
        let path = root.join("results").join(format!(
            "micro-persisted-{date}-{}.json",
            self.hardware.host_id
        ));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, format!("{json}\n"))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(path)
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "{:<14} {:>7} {:>14} {:>12} {:>14} {:>10} {:>12}\n",
            "construct", "steps", "µs/instance", "µs/step", "marginal µs", "events", "bytes/inst"
        ));
        for c in &self.constructs {
            out.push_str(&format!(
                "{:<14} {:>7.1} {:>14.1} {:>12.1} {:>14.1} {:>10.1} {:>12.0}\n",
                c.name,
                c.steps_per_instance,
                c.micros_per_instance,
                c.micros_per_step,
                c.marginal_micros_vs_baseline,
                c.events_per_instance,
                c.bytes_per_instance,
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shape_compiles() {
        // The persisted suite deploys these against a database; that they
        // are executable models at all is checkable without one, and this is
        // where a generator change that breaks a shape gets caught.
        for (name, block) in shapes() {
            let generated = build(&block);
            let xml = with_process_id(&generated.xml, name);
            let definitions = rbpmn_model::parse(&xml).expect("valid BPMN");
            rbpmn_core::ExecutableProcess::compile(
                &definitions,
                name,
                &rbpmn_core::Bindings::default(),
            )
            .unwrap_or_else(|e| panic!("the {name} shape must compile: {e}"));
        }
    }

    #[test]
    fn the_process_id_is_actually_rewritten() {
        let xml = build(&Block::Task).xml;
        let renamed = with_process_id(&xml, "single-task");
        assert!(renamed.contains("<bpmn:process id=\"single-task\""));
        assert!(!renamed.contains("<bpmn:process id=\"p\""));
    }
}
