//! The lifecycle benchmark: generate, execute, monitor — and `steady` for
//! latency under a fixed arrival rate.
//!
//! The three-mode split is not ceremony. `generate` parks a backlog with the
//! workers switched off, so `execute` measures a **saturated** system from
//! its first millisecond instead of averaging in a ramp-up; `monitor` runs
//! in its own process so its sampling cannot be scheduled behind the load it
//! is sampling. Drain-the-backlog answers "how many instances per second",
//! and only that: for "how long does an instance take when the system is
//! busy but not saturated" there is `steady`, whose arrivals are open-loop
//! and whose lateness is recorded rather than absorbed.
//!
//! One iteration is a whole instance lifecycle — started, work items
//! claimed, merge patches applied, terminal state reached — because that is
//! the thing an application cares about. Instance creation alone is the
//! number vendors quote and it is not a workload.

use crate::model::Model;
use crate::pg;
use crate::result::{
    Backpressure, HarnessConfig, Latency, MAX_RAW_LATENCIES, Measurements, Provenance, RunResult,
    SCHEMA, ScopeNotes,
};
use crate::scenario::Scenario;
use crate::vars;
use rbpmn_engine::{
    Engine, EngineError, GetTaskOptions, HandlerFailure, SchedulerOptions, ServiceTaskHandler,
    TaskFilter, WorkItem, WorkerOptions,
};
use sqlx::{PgPool, Row};
use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Park a backlog, then drain it. Reports saturation throughput.
    Saturation,
    /// Open-loop arrivals at a fixed rate. Reports latency under load.
    Steady,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Saturation => "saturation",
            Mode::Steady => "steady",
        }
    }
}

/// Which halves of the run to perform. The split exists so a backlog can be
/// parked by one invocation and drained — and measured — by another: same
/// database, same deterministic seeding, no state file between them beyond
/// the run id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phases {
    /// Park the backlog and stop. No workers are started.
    Generate,
    /// Drain a backlog someone else parked, and measure it.
    Execute,
    /// Warm up, park, drain, measure — what `just bench` runs.
    Both,
}

impl Phases {
    fn generates(self) -> bool {
        matches!(self, Phases::Generate | Phases::Both)
    }
}

pub struct RunOptions {
    pub root: PathBuf,
    pub database_url: String,
    pub provisioned_by: String,
    pub mode: Mode,
    pub phases: Phases,
    /// Set when `execute` must find the backlog a previous `generate` left.
    pub run_id: Option<String>,
    pub monitor_interval_secs: Option<f64>,
    /// Abort when nothing has completed for this long. A benchmark that
    /// hangs forever is worse than one that fails: it burns the machine and
    /// tells you nothing.
    pub stall_timeout: Duration,
    /// Overrides the scenario's instance count (quick smoke runs).
    pub instances: Option<u32>,
    pub warmup: Option<u32>,
    /// Empty every rbpmn table before the run. On by default, and the reason
    /// is comparability: a benchmark whose numbers depend on how many
    /// previous runs happened to be left lying around is not a benchmark.
    /// `--no-fresh` keeps the data — see the README's note on what that
    /// exposes.
    pub fresh: bool,
    /// `ANALYZE` the instance and work-item tables after the backlog is
    /// parked and before the workers start. See [`analyze_before_execute`].
    pub analyze: bool,
}

/// Why the harness runs `ANALYZE` before it measures, and why that is not
/// tuning the numbers.
///
/// The claim path joins `rbpmn_work_item` to `rbpmn_instance` and filters on
/// `i.status = 'active'`. When the planner's statistics were last collected
/// on an idle system, `status` looks 100% `completed` — so it estimates that
/// *no* instance is active, drives the nested loop from `rbpmn_instance`,
/// and bitmap-scans work items per instance. That is O(active instances) per
/// claim, on the hot path.
///
/// Measured, on one database, same code, same parked backlog of 300
/// instances, the only difference being one `ANALYZE`:
///
/// ```text
/// stale statistics ({completed}, freq 1.00)      →  20.6 instances/sec
/// current statistics ({completed 0.89, active 0.11}) → 175.4 instances/sec
/// ```
///
/// So without this the benchmark would mostly be measuring **when autovacuum
/// last ran**, which is not a property of the engine. With it, the
/// measurement is the engine.
///
/// The effect itself is worth knowing about — it is a real hazard for any
/// deployment whose instance table goes quiet and then gets a burst — and it
/// reproduces exactly:
///
/// ```text
/// psql -c 'analyze rbpmn_instance, rbpmn_work_item'   # while idle: status looks 100% completed
/// rbpmn-bench generate mixed-typical --instances 300 --run-id repro --no-fresh
/// rbpmn-bench execute  mixed-typical --instances 300 --run-id repro --no-analyze
/// ```
///
/// `--no-analyze` alone does not do it: a fresh database has no statistics
/// to be stale, and the planner's defaults happen to pick the right plan.
/// The result file records both flags.
async fn analyze_before_execute(pool: &PgPool) -> Result<(), String> {
    sqlx::query("analyze rbpmn_instance, rbpmn_work_item")
        .execute(pool)
        .await
        .map_err(|e| format!("analyzing before the measured phase: {e}"))?;
    Ok(())
}

/// Empty every rbpmn table except the ones holding **seeded singleton state**
/// rather than data. Enumerated from the catalogue rather than listed, so a
/// table added by a future migration cannot silently survive a "fresh" run.
///
/// `rbpmn_retention_floor` is on the exclusion list because migration 0007
/// seeds exactly one row into it and the event stream refuses to release any
/// page without it — truncating it broke `read_events` with a loud and
/// entirely correct error, which is how the omission was found. Its *value*
/// is still reset below: a fresh database has deleted nothing, so its
/// truncation floor is zero.
pub(crate) async fn truncate_all(pool: &PgPool) -> Result<u64, String> {
    let tables: Vec<String> = sqlx::query_scalar(
        "select relname from pg_class \
         where relname like 'rbpmn\\_%' and relkind = 'r' \
           and relname not in ('rbpmn_migrations', 'rbpmn_environment_topic', \
                               'rbpmn_retention_floor') \
         order by relname",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("listing rbpmn tables: {e}"))?;
    if tables.is_empty() {
        return Ok(0);
    }
    // `restart identity` resets rbpmn_event's bigserial too: two runs of the
    // same scenario should produce the same event ids, not ids that depend
    // on how much ran before them.
    sqlx::query(&format!(
        "truncate {} restart identity cascade",
        tables.join(", ")
    ))
    .execute(pool)
    .await
    .map_err(|e| format!("truncating for a fresh run: {e}"))?;
    // Nothing has been deleted from an empty database, so the floor is zero
    // — but the row itself must survive, or the event stream cannot state a
    // floor and correctly refuses to serve a page at all.
    sqlx::query("update rbpmn_retention_floor set txid = '0'::xid8, id = 0")
        .execute(pool)
        .await
        .map_err(|e| format!("resetting the truncation floor: {e}"))?;
    Ok(tables.len() as u64)
}

/// A run's shared counters. Everything the harness itself did, so the result
/// file can state the work behind the throughput rather than implying it.
#[derive(Default)]
struct Counters {
    work_items: AtomicU64,
    inbox_queries: AtomicU64,
    correlated: AtomicU64,
    correlate_retries: AtomicU64,
    handler_failures: AtomicU64,
}

/// The push-mode handler. Returns the scenario's merge patch; it does no
/// work of its own on purpose — an in-process handler that slept would be
/// measuring the sleep, and the README says out loud that no network
/// latency is in these numbers.
struct BenchHandler {
    patch: serde_json::Value,
    counters: Arc<Counters>,
}

impl ServiceTaskHandler for BenchHandler {
    fn execute(
        &self,
        _item: WorkItem,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, HandlerFailure>> + Send + '_>> {
        let patch = self.patch.clone();
        self.counters.work_items.fetch_add(1, Ordering::Relaxed);
        Box::pin(async move { Ok(patch) })
    }
}

/// `None` is the `generate` phase's success: a backlog is parked, nothing
/// has been measured, and inventing a result file for it would be a lie.
pub async fn run(scenario: &Scenario, options: &RunOptions) -> Result<Option<RunResult>, String> {
    if let Some(reason) = scenario.workload.history.unsupported_reason() {
        return Err(reason);
    }
    let model = Model::load(&scenario.model_path(&options.root))?;
    let bindings = scenario.bindings(&model);
    let warnings_from_check = crate::model::check(&model, &bindings)?;

    let warmup = match options.phases {
        Phases::Both => options.warmup.unwrap_or(scenario.workload.warmup),
        // A `generate` parks a backlog; a warmup pass would drain instances
        // through workers that this phase is defined as not running. And an
        // `execute` measures a backlog that is already there — warming up
        // now would warm the caches *after* the thing being measured was
        // written.
        _ => 0,
    };
    let measured = options.instances.unwrap_or(scenario.workload.instances);
    let run_id = options
        .run_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string()[..8].to_string());
    let started_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    // The pool must hold every worker's connection *and* the listeners they
    // establish when they go idle, or the benchmark deadlocks on itself and
    // reports it as slowness.
    let workers = scenario.execute.service_workers
        + scenario.execute.user_workers
        + scenario.execute.correlators;
    let needed = workers * 2 + 4;
    if scenario.execute.db_pool < needed {
        return Err(format!(
            "{}: db_pool = {} is too small for {workers} worker loops: each can hold a \
             working connection and a LISTEN connection at the same time, so at least \
             {needed} are needed. A benchmark starved of connections measures its own \
             pool, not the engine.",
            scenario.name, scenario.execute.db_pool
        ));
    }

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(scenario.execute.db_pool)
        .acquire_timeout(Duration::from_secs(30))
        .connect(&options.database_url)
        .await
        .map_err(|e| format!("connecting to {}: {e}", redact(&options.database_url)))?;

    let counters = Arc::new(Counters::default());
    let patch = scenario
        .execute
        .patch
        .clone()
        .unwrap_or_else(|| serde_json::json!({ "handled": true }));

    let mut builder = Engine::builder(pool.clone());
    for topic in scenario.service_topics(&model) {
        builder = builder.handler(
            topic,
            Arc::new(BenchHandler {
                patch: patch.clone(),
                counters: counters.clone(),
            }),
        );
    }
    let engine = builder.build();

    engine
        .migrate()
        .await
        .map_err(|e| format!("migrate: {e}"))?;
    // Before the deploy, or the definition this run is about to use would be
    // the thing truncated. Only for the phases that own the whole run: an
    // `execute` exists precisely to drain a backlog someone else parked.
    if options.fresh && options.phases != Phases::Execute {
        truncate_all(&pool).await?;
    }
    settle(&pool).await;
    let tuning = apply_tuning(&pool, &options.root).await?;
    engine
        .sync_environment()
        .await
        .map_err(|e| format!("sync_environment: {e}"))?;

    let deployment = engine
        .deploy(&model.xml, &bindings)
        .await
        .map_err(|e| format!("deploy {}: {e}", model.file))?;

    let facts = pg::facts(
        &pool,
        &host_of(&options.database_url),
        &options.provisioned_by,
    )
    .await
    .map_err(|e| format!("capturing postgres settings: {e}"))?;
    let hardware = crate::env::Hardware::detect(&options.root);
    let checkout = crate::env::Checkout::detect(&options.root);

    let mut warnings: Vec<String> = Vec::new();
    warnings.extend(warnings_from_check.iter().map(|w| format!("lint: {w}")));
    warnings.extend(hardware.declaration_mismatches.iter().cloned());
    if checkout.dirty {
        warnings.push(
            "the checkout has uncommitted changes: this result is not reproducible from \
             its git sha alone"
                .to_string(),
        );
    }
    if deployment.reused {
        warnings.push(format!(
            "definition {} v{} was already deployed with identical content — the database \
             is not empty, so table sizes and cache state include earlier runs",
            deployment.key, deployment.version
        ));
    }

    // ---------------------------------------------------------- warmup pass
    //
    // A whole pass, run and thrown away: caches warm, the planner has
    // statistics, the pool is established, and the JIT-free steady state has
    // been reached. Its instances stay in the database, because the table
    // sizes they leave behind are part of the system under test.
    if warmup > 0 {
        let phase = Phase {
            marker: format!("{run_id}:w"),
            count: warmup,
        };
        drive(
            &engine,
            &pool,
            scenario,
            &model,
            &phase,
            &run_id,
            counters.clone(),
            Mode::Saturation,
            Phases::Both,
            // The warmup pass is thrown away; analyzing before it would just
            // record statistics that the pass itself invalidates.
            false,
            options.stall_timeout,
        )
        .await
        .map_err(|e| format!("warmup: {e}"))?;
    }

    if options.phases == Phases::Generate {
        let phase = Phase {
            marker: format!("{run_id}:m"),
            count: measured,
        };
        generate_backlog(&engine, scenario, &model, &phase, &run_id).await?;
        println!(
            "parked {measured} instances of '{}' — drain and measure them with:\n  \
             rbpmn-bench execute {} --run-id {run_id} --instances {measured}",
            scenario.name, scenario.name
        );
        return Ok(None);
    }

    // -------------------------------------------------------- measured pass
    let monitor = match options.monitor_interval_secs {
        Some(interval) => Some(Monitor::spawn(&options.database_url, interval)?),
        None => None,
    };

    let phase = Phase {
        marker: format!("{run_id}:m"),
        count: measured,
    };
    let outcome = drive(
        &engine,
        &pool,
        scenario,
        &model,
        &phase,
        &run_id,
        counters.clone(),
        options.mode,
        options.phases,
        options.analyze,
        options.stall_timeout,
    )
    .await?;

    let samples = match monitor {
        Some(monitor) => monitor.finish().await?,
        None => Vec::new(),
    };

    // ------------------------------------------------------------- measure
    let latencies = latencies_ms(&pool, &phase.marker, outcome.latency_from)
        .await
        .map_err(|e| format!("reading instance latencies: {e}"))?;
    let counts = phase_counts(&pool, &phase.marker)
        .await
        .map_err(|e| format!("counting instances: {e}"))?;
    let (event_rows, event_bytes) = pg::event_volume(&pool)
        .await
        .map_err(|e| format!("measuring event volume: {e}"))?;
    let all_instances: i64 = sqlx::query_scalar("select count(*) from rbpmn_instance")
        .fetch_one(&pool)
        .await
        .map_err(|e| format!("counting instances: {e}"))?;

    let mut sorted = latencies;
    sorted.sort_by(|a, b| a.partial_cmp(b).expect("latencies are finite"));
    let latency = Latency::from_sorted(&sorted);
    let truncated = sorted.len() > MAX_RAW_LATENCIES;
    let raw: Vec<f64> = sorted
        .iter()
        .take(MAX_RAW_LATENCIES)
        .map(|ms| (ms * 100.0).round() / 100.0)
        .collect();

    let handler_failures = counters.handler_failures.load(Ordering::Relaxed);
    if handler_failures > 0 {
        warnings.push(format!(
            "{handler_failures} handler invocations failed — the measured system was not \
             the healthy one"
        ));
    }

    let measurements = Measurements {
        measured_instances: measured,
        completed_instances: counts.completed,
        failed_instances: counts.failed,
        duration_secs: outcome.duration.as_secs_f64(),
        throughput_instances_per_sec: counts.completed as f64 / outcome.duration.as_secs_f64(),
        latency_kind: outcome.latency_kind.to_string(),
        latency_ms: latency,
        latencies_ms: raw,
        latencies_truncated: truncated,
        work_items_completed: counters.work_items.load(Ordering::Relaxed),
        inbox_queries: counters.inbox_queries.load(Ordering::Relaxed),
        messages_correlated: counters.correlated.load(Ordering::Relaxed),
        correlate_retries: counters.correlate_retries.load(Ordering::Relaxed),
        events_written: event_rows,
        // Per *instance in the database*, not per measured instance: the
        // warmup pass wrote events too, and dividing one pass's rows by
        // another pass's instances would be a made-up number.
        events_per_instance: div(event_rows as f64, all_instances as f64),
        event_bytes_per_instance: div(event_bytes as f64, all_instances as f64),
        backpressure: outcome.backpressure,
    };

    let mut models = std::collections::BTreeMap::new();
    models.insert(model.file.clone(), model.sha256.clone());

    Ok(Some(RunResult {
        schema: SCHEMA.to_string(),
        scenario: scenario.name.clone(),
        mode: options.mode.as_str().to_string(),
        history: scenario.workload.history.as_str().to_string(),
        run_id,
        started_at,
        checkout,
        hardware,
        postgres: facts,
        provenance: Provenance {
            scenario_sha256: crate::result::scenario_hash(
                &options
                    .root
                    .join("scenarios")
                    .join(format!("{}.toml", scenario.name)),
            )?,
            models,
            tuning_sha256: tuning,
            seed: scenario.workload.seed,
            bindings: serde_json::to_value(&bindings).map_err(|e| e.to_string())?,
        },
        scope: ScopeNotes::new(scenario, &model),
        harness: HarnessConfig {
            service_workers: scenario.execute.service_workers,
            user_workers: scenario.execute.user_workers,
            correlators: scenario.execute.correlators,
            schedulers: 1,
            db_pool: scenario.execute.db_pool,
            warmup_instances: warmup,
            measured_instances: measured,
            monitor_interval_secs: options.monitor_interval_secs,
            fresh_database: options.fresh,
            analyze_before_execute: options.analyze,
            arrival_rate: (options.mode == Mode::Steady)
                .then(|| scenario.steady.as_ref().map(|s| s.arrival_rate))
                .flatten(),
            steady_duration_secs: (options.mode == Mode::Steady)
                .then(|| scenario.steady.as_ref().map(|s| s.duration_secs))
                .flatten(),
        },
        measurements,
        monitor: samples,
        warnings,
    }))
}

fn div(a: f64, b: f64) -> f64 {
    if b == 0.0 { 0.0 } else { a / b }
}

struct Phase {
    /// Business-key prefix identifying this pass's instances. `business_key`
    /// is free-form application data the engine never interprets, which
    /// makes it exactly the right place to tag a batch — and it leaves the
    /// run auditable in the database afterwards.
    marker: String,
    count: u32,
}

struct Outcome {
    duration: Duration,
    /// Database timestamp latencies are measured from.
    latency_from: LatencyFrom,
    latency_kind: &'static str,
    backpressure: Backpressure,
}

#[derive(Clone, Copy)]
enum LatencyFrom {
    /// `completed_at - created_at`: the instance's own lifetime.
    Arrival,
    /// `completed_at - <drain start>`: queue-inclusive, saturation only.
    Instant(chrono::DateTime<chrono::Utc>),
}

/// One pass: park the backlog (or open the arrival tap), run the workers,
/// wait for every instance to reach a terminal state.
#[allow(clippy::too_many_arguments)]
async fn drive(
    engine: &Engine,
    pool: &PgPool,
    scenario: &Scenario,
    model: &Model,
    phase: &Phase,
    run_id: &str,
    counters: Arc<Counters>,
    mode: Mode,
    phases: Phases,
    analyze: bool,
    stall_timeout: Duration,
) -> Result<Outcome, String> {
    let keys = correlation_keys(scenario, run_id, phase.count);

    match mode {
        Mode::Saturation => {
            // ------ generate: park the whole backlog, workers switched off
            if phases.generates() {
                generate_backlog(engine, scenario, model, phase, run_id).await?;
            } else {
                let counts = phase_counts(pool, &phase.marker)
                    .await
                    .map_err(|e| format!("looking for the parked backlog: {e}"))?;
                if counts.active == 0 {
                    return Err(format!(
                        "no active instances tagged '{}:%' — nothing to drain. Did the \
                         `generate` phase run against this database, with this run id?",
                        phase.marker
                    ));
                }
            }
            if analyze {
                analyze_before_execute(pool).await?;
            }
            let drain_start: chrono::DateTime<chrono::Utc> =
                sqlx::query_scalar("select clock_timestamp()")
                    .fetch_one(pool)
                    .await
                    .map_err(|e| format!("reading database time: {e}"))?;

            // ------------------------------------- execute: drain and time
            let started = Instant::now();
            let crew = Crew::spawn(engine, scenario, &model.key, counters.clone(), keys);
            let result = wait_for_drain(pool, &model.key, &phase.marker, stall_timeout).await;
            let wall = started.elapsed();
            crew.stop();
            result?;
            // Database time, not the poll loop's: the drain ends when the
            // last instance committed, which is a fact in the database, not
            // the moment a 20 ms poll happened to notice. Falls back to the
            // wall clock only if nothing completed at all.
            let duration = span_since(pool, &phase.marker, drain_start)
                .await
                .map_err(|e| format!("measuring the drain span: {e}"))?
                .unwrap_or(wall);
            Ok(Outcome {
                duration,
                latency_from: LatencyFrom::Instant(drain_start),
                latency_kind: "drain",
                backpressure: Backpressure {
                    occurred: false,
                    reason: "saturation mode has no arrival process to fall behind: the \
                             backlog is parked before the workers start"
                        .to_string(),
                    max_arrival_lag_ms: None,
                    arrivals_late: None,
                },
            })
        }
        Mode::Steady => {
            let steady = scenario.steady.as_ref().ok_or_else(|| {
                format!(
                    "{}: steady mode needs a [steady] section (arrival_rate, duration_secs)",
                    scenario.name
                )
            })?;
            if analyze {
                analyze_before_execute(pool).await?;
            }
            let crew = Crew::spawn(engine, scenario, &model.key, counters.clone(), keys);
            let started = Instant::now();
            let interval = Duration::from_secs_f64(1.0 / steady.arrival_rate);
            let planned = (steady.arrival_rate * steady.duration_secs as f64).ceil() as u32;
            let mut late = 0u64;
            let mut max_lag = 0.0f64;
            for index in 0..planned {
                let due = started + interval.mul_f64(index as f64);
                let now = Instant::now();
                if now < due {
                    tokio::time::sleep(due - now).await;
                } else {
                    // Open loop: an arrival that could not be issued on time
                    // is recorded, never absorbed by slowing the tap down.
                    let lag = (now - due).as_secs_f64() * 1000.0;
                    if lag > 1.0 {
                        late += 1;
                        max_lag = max_lag.max(lag);
                    }
                }
                start_instance(engine, scenario, model, phase, run_id, index).await?;
            }
            let result = wait_for_drain(pool, &model.key, &phase.marker, stall_timeout).await;
            let wall = started.elapsed();
            crew.stop();
            result?;
            // From the first arrival's commit to the last completion, in
            // database time — the window the throughput figure divides by.
            let duration = arrival_span(pool, &phase.marker)
                .await
                .map_err(|e| format!("measuring the arrival span: {e}"))?
                .unwrap_or(wall);
            Ok(Outcome {
                duration,
                latency_from: LatencyFrom::Arrival,
                latency_kind: "arrival",
                backpressure: Backpressure {
                    occurred: late > 0,
                    reason: if late > 0 {
                        format!(
                            "{late} of {planned} arrivals were issued late — the harness \
                             could not sustain {} instances/sec, so the latency figures \
                             describe a lower rate than the one requested",
                            steady.arrival_rate
                        )
                    } else {
                        format!(
                            "every arrival was issued on schedule at {} instances/sec",
                            steady.arrival_rate
                        )
                    },
                    max_arrival_lag_ms: Some(max_lag),
                    arrivals_late: Some(late),
                },
            })
        }
    }
}

/// Seconds from `from` to this pass's last completion, in database time.
/// `None` when nothing completed.
async fn span_since(
    pool: &PgPool,
    marker: &str,
    from: chrono::DateTime<chrono::Utc>,
) -> Result<Option<Duration>, sqlx::Error> {
    let seconds: Option<f64> = sqlx::query_scalar(
        "select extract(epoch from (max(completed_at) - $2))::float8 \
         from rbpmn_instance where business_key like $1",
    )
    .bind(format!("{marker}:%"))
    .bind(from)
    .fetch_one(pool)
    .await?;
    Ok(seconds.filter(|s| *s > 0.0).map(Duration::from_secs_f64))
}

/// First arrival to last completion, in database time.
async fn arrival_span(pool: &PgPool, marker: &str) -> Result<Option<Duration>, sqlx::Error> {
    let seconds: Option<f64> = sqlx::query_scalar(
        "select extract(epoch from (max(completed_at) - min(created_at)))::float8 \
         from rbpmn_instance where business_key like $1",
    )
    .bind(format!("{marker}:%"))
    .fetch_one(pool)
    .await?;
    Ok(seconds.filter(|s| *s > 0.0).map(Duration::from_secs_f64))
}

/// The `generate` phase: park `count` instances at their first wait state
/// with no worker running, so `execute` meets a saturated system rather than
/// a ramp-up.
async fn generate_backlog(
    engine: &Engine,
    scenario: &Scenario,
    model: &Model,
    phase: &Phase,
    run_id: &str,
) -> Result<(), String> {
    for index in 0..phase.count {
        start_instance(engine, scenario, model, phase, run_id, index).await?;
    }
    Ok(())
}

async fn start_instance(
    engine: &Engine,
    scenario: &Scenario,
    model: &Model,
    phase: &Phase,
    run_id: &str,
    index: u32,
) -> Result<(), String> {
    let variables = vars::document(
        &scenario.workload.fields,
        scenario.workload.seed,
        run_id,
        index,
    );
    let business_key = format!("{}:{index}", phase.marker);
    engine
        .start(&model.key, Some(&business_key), variables)
        .await
        .map_err(|e| format!("start {} #{index}: {e}", model.key))?;
    Ok(())
}

fn correlation_keys(scenario: &Scenario, run_id: &str, count: u32) -> VecDeque<String> {
    let Some(correlation) = &scenario.bindings.correlation else {
        return VecDeque::new();
    };
    // The key is *derived* from (run id, index), the same way the variable
    // document derived it — so the correlator knows what to deliver without
    // reading anything back, exactly as an application that started the
    // instance would.
    let prefix = scenario
        .workload
        .fields
        .iter()
        .find(|f| f.path == correlation.key)
        .and_then(|f| match &f.value {
            crate::scenario::FieldValue::Unique { prefix } => Some(prefix.clone()),
            _ => None,
        })
        .unwrap_or_default();
    (0..count)
        .map(|index| vars::unique(&prefix, run_id, index))
        .collect()
}

/// Everything that consumes work during a pass: push workers, the scheduler,
/// pull-mode inbox workers, and the correlator. Dropped by `stop`, which
/// aborts them — the loops run forever by design.
struct Crew(Vec<tokio::task::JoinHandle<()>>);

impl Crew {
    fn spawn(
        engine: &Engine,
        scenario: &Scenario,
        definition_key: &str,
        counters: Arc<Counters>,
        keys: VecDeque<String>,
    ) -> Crew {
        let mut handles = Vec::new();
        for n in 0..scenario.execute.service_workers {
            let engine = engine.clone();
            handles.push(tokio::spawn(async move {
                engine
                    .run_worker(WorkerOptions {
                        owner: format!("bench-service-{n}"),
                        lease: Duration::from_secs(60),
                        poll_interval: Duration::from_millis(200),
                    })
                    .await;
            }));
        }

        // Always on: a timer scenario needs it, and an idle scheduler is a
        // sleeping loop woken by NOTIFY — measuring the engine as deployed
        // beats measuring a stripped-down variant of it.
        {
            let engine = engine.clone();
            handles.push(tokio::spawn(async move {
                engine
                    .run_scheduler(SchedulerOptions {
                        poll_interval: Duration::from_millis(200),
                    })
                    .await;
            }));
        }

        if let (Some(topic), true) = (scenario.user_topic(), scenario.execute.user_workers > 0) {
            let query = scenario.execute.user_query.clone();
            let definition_key = definition_key.to_string();
            let patch = scenario
                .execute
                .patch
                .clone()
                .unwrap_or_else(|| serde_json::json!({ "handled": true }));
            for n in 0..scenario.execute.user_workers {
                let engine = engine.clone();
                let counters = counters.clone();
                let topic = topic.to_string();
                let query = query.clone();
                let definition_key = definition_key.clone();
                let patch = patch.clone();
                handles.push(tokio::spawn(async move {
                    inbox_worker(engine, n, topic, definition_key, query, patch, counters).await;
                }));
            }
        }

        if scenario.execute.correlators > 0
            && let Some(correlation) = scenario.bindings.correlation.clone()
        {
            let queue = Arc::new(tokio::sync::Mutex::new(keys));
            for _ in 0..scenario.execute.correlators {
                let engine = engine.clone();
                let counters = counters.clone();
                let queue = queue.clone();
                let correlation = correlation.clone();
                handles.push(tokio::spawn(async move {
                    correlator(engine, correlation, queue, counters).await;
                }));
            }
        }

        Crew(handles)
    }

    fn stop(self) {
        for handle in self.0 {
            handle.abort();
        }
    }
}

/// A task frontend, as a real one behaves: it *finds* work before it does
/// work. `count_tasks` is the dashboard indication next to the list, and the
/// claim is filtered — both are inside the measured path because leaving the
/// query out is how a benchmark flatters itself.
async fn inbox_worker(
    engine: Engine,
    n: u32,
    topic: String,
    definition_key: String,
    query: Option<crate::scenario::UserQuery>,
    patch: serde_json::Value,
    counters: Arc<Counters>,
) {
    let owner = format!("bench-inbox-{n}");
    // Workers partition themselves across the filter values, so the whole
    // inbox drains instead of one slice of it.
    let filter = query.as_ref().map(|q| {
        let value = &q.filter_values[n as usize % q.filter_values.len()];
        TaskFilter::new(&definition_key).field(&q.filter_field, value)
    });
    let count_first = query.as_ref().is_some_and(|q| q.count_first);
    // Backing off on an empty inbox is not a detail — it decides what is
    // being measured. A frontend that re-queried every 2 ms would put two
    // scans per worker per 2 ms on the database and the benchmark would be
    // reporting the *harness's* polling, not the engine's throughput. (It
    // did: mixed-typical read 32 instances/sec that way and 100+ once this
    // backed off.) Real inboxes poll on human timescales; this one backs off
    // to 50 ms, which is still far more eager than any of them.
    let mut idle = Duration::from_millis(1);
    loop {
        if count_first {
            counters.inbox_queries.fetch_add(1, Ordering::Relaxed);
            if engine.count_tasks(&topic, filter.as_ref()).await.is_err() {
                tokio::time::sleep(Duration::from_millis(20)).await;
                continue;
            }
        }
        let mut options = GetTaskOptions::new(&owner);
        options.ttl = Duration::from_secs(60);
        options.filter = filter.clone();
        counters.inbox_queries.fetch_add(1, Ordering::Relaxed);
        match engine.get_task(&topic, &options).await {
            Ok(Some(task)) => {
                idle = Duration::from_millis(1);
                match engine.complete_task(task.id, &owner, patch.clone()).await {
                    Ok(_) => {
                        counters.work_items.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(_) => {
                        counters.handler_failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Ok(None) => {
                tokio::time::sleep(idle).await;
                idle = (idle * 2).min(Duration::from_millis(50));
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
}

/// Delivers the messages the instances are waiting for. A key whose
/// subscription is not armed yet goes back on the queue — the catch is not
/// the first element in the model on purpose, so this is the ordinary case
/// rather than an error, and the retries are counted and reported.
async fn correlator(
    engine: Engine,
    correlation: crate::scenario::CorrelationSpec,
    queue: Arc<tokio::sync::Mutex<VecDeque<String>>>,
    counters: Arc<Counters>,
) {
    // Same reasoning as the inbox worker: a key whose subscription is not
    // armed yet must not be retried in a tight loop, or the correlator's
    // failed lookups become the workload.
    let mut idle = Duration::from_millis(1);
    loop {
        let Some(key) = queue.lock().await.pop_front() else {
            tokio::time::sleep(Duration::from_millis(5)).await;
            continue;
        };
        match engine
            .correlate(
                &correlation.message,
                &key,
                serde_json::json!({ "paid": true }),
            )
            .await
        {
            Ok(_) => {
                idle = Duration::from_millis(1);
                counters.correlated.fetch_add(1, Ordering::Relaxed);
            }
            Err(EngineError::NoSubscription { .. }) => {
                counters.correlate_retries.fetch_add(1, Ordering::Relaxed);
                queue.lock().await.push_back(key);
                tokio::time::sleep(idle).await;
                idle = (idle * 2).min(Duration::from_millis(25));
            }
            Err(_) => {
                counters.handler_failures.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

struct PhaseCounts {
    active: i64,
    completed: i64,
    failed: i64,
}

/// The exact per-phase accounting, by business key. A `like` over
/// `business_key` is a sequential scan — `business_key` is nullable,
/// unindexed and non-unique by design — so this runs **once, at the end of a
/// pass**, never in the polling loop. It used to be the poll, and the cost
/// grew with the database rather than with the run: a scenario measured
/// 200 instances/sec on a fresh database and 27 on one holding a few
/// thousand instances, entirely because of this query.
async fn phase_counts(pool: &PgPool, marker: &str) -> Result<PhaseCounts, sqlx::Error> {
    let row = sqlx::query(
        "select \
           count(*) filter (where status = 'active') as active, \
           count(*) filter (where status in ('completed','terminated')) as completed, \
           count(*) filter (where status = 'failed') as failed \
         from rbpmn_instance where business_key like $1",
    )
    .bind(format!("{marker}:%"))
    .fetch_one(pool)
    .await?;
    Ok(PhaseCounts {
        active: row.get("active"),
        completed: row.get("completed"),
        failed: row.get("failed"),
    })
}

/// The polling form: indexed on `(definition_key, status)`, so it costs what
/// is *left to do* rather than what the database has ever held. Safe as a
/// stand-in for the per-phase count because a run drains its warmup pass
/// before the measured one begins, so the only live instances of this
/// definition are this pass's.
async fn live_counts(pool: &PgPool, definition_key: &str) -> Result<(i64, i64), sqlx::Error> {
    let row = sqlx::query(
        "select \
           (select count(*) from rbpmn_instance \
             where definition_key = $1 and status = 'active') as active, \
           (select count(*) from rbpmn_instance \
             where definition_key = $1 and status = 'failed') as failed",
    )
    .bind(definition_key)
    .fetch_one(pool)
    .await?;
    Ok((row.get("active"), row.get("failed")))
}

/// Wait until this pass's instances are all terminal.
///
/// Two things end it early, both loudly: an instance that froze on an
/// incident (the measured system was broken, and averaging over a broken
/// system is worse than no number), and a stall — no progress for
/// `stall_timeout`.
async fn wait_for_drain(
    pool: &PgPool,
    definition_key: &str,
    marker: &str,
    stall_timeout: Duration,
) -> Result<(), String> {
    let mut last_progress = Instant::now();
    let mut last_active = i64::MAX;
    loop {
        let (active, failed) = live_counts(pool, definition_key)
            .await
            .map_err(|e| format!("polling drain progress: {e}"))?;
        if failed > 0 {
            return Err(incident_report(pool, marker, failed).await);
        }
        if active == 0 {
            return Ok(());
        }
        if active != last_active {
            last_active = active;
            last_progress = Instant::now();
        } else if last_progress.elapsed() > stall_timeout {
            return Err(format!(
                "stalled: {active} instances still active and none finished for {:?}. \
                 Something is not being consumed — check that every topic in the \
                 scenario has a worker.",
                stall_timeout
            ));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A frozen instance is evidence, so say what froze and where rather than
/// "the benchmark failed".
async fn incident_report(pool: &PgPool, marker: &str, failed: i64) -> String {
    let detail = sqlx::query(
        "select i.id::text as id, coalesce(t.element_id, '?') as element, \
                coalesce(w.last_failure, '(no detail recorded)') as failure \
         from rbpmn_instance i \
         left join rbpmn_token t on t.instance_id = i.id and t.wait_kind = 'incident' \
         left join rbpmn_work_item w on w.instance_id = i.id and w.state = 'failed' \
         where i.business_key like $1 and i.status = 'failed' limit 3",
    )
    .bind(format!("{marker}:%"))
    .fetch_all(pool)
    .await;
    let mut out = format!("{failed} instances froze on an incident during the run");
    if let Ok(rows) = detail {
        for row in rows {
            out.push_str(&format!(
                "\n  instance {} at element '{}': {}",
                row.get::<String, _>("id"),
                row.get::<String, _>("element"),
                row.get::<String, _>("failure"),
            ));
        }
    }
    out.push_str("\n  inspect one with the read-only inspector before trusting any number.");
    out
}

/// Per-instance latencies, computed in the database over database
/// timestamps: no client clock, no skew between the process that started an
/// instance and the process that finished it.
async fn latencies_ms(
    pool: &PgPool,
    marker: &str,
    from: LatencyFrom,
) -> Result<Vec<f64>, sqlx::Error> {
    let sql = match from {
        LatencyFrom::Arrival => {
            "select (extract(epoch from (completed_at - created_at)) * 1000)::float8 as ms \
             from rbpmn_instance \
             where business_key like $1 and completed_at is not null"
        }
        LatencyFrom::Instant(_) => {
            "select (extract(epoch from (completed_at - $2)) * 1000)::float8 as ms \
             from rbpmn_instance \
             where business_key like $1 and completed_at is not null"
        }
    };
    let query = sqlx::query_scalar::<_, f64>(sql).bind(format!("{marker}:%"));
    let query = match from {
        LatencyFrom::Arrival => query,
        LatencyFrom::Instant(at) => query.bind(at),
    };
    query.fetch_all(pool).await
}

/// Force a checkpoint and let the background workers catch up before a
/// scenario starts.
///
/// Suite hygiene, and it is not optional. Running the scenarios back to back
/// without this, the *last* two in the suite measured roughly half what they
/// measured standing alone — 327 against 574-655 instances/sec for
/// `timer-short`, 1186 against 1444-1537 for `usertask-inbox` — because each
/// scenario starts while the checkpointer and autovacuum are still working
/// through the previous one's writes. Truncating the tables does not undo
/// that; the debt is in the WAL and the background workers, not the rows.
///
/// A benchmark whose numbers depend on alphabetical position is not
/// measuring the engine. Failure here is tolerated with a warning rather
/// than fatal: `CHECKPOINT` needs superuser or `pg_checkpoint`, and a run
/// against a database where the caller lacks it should still produce
/// numbers — noisier ones, honestly labelled.
async fn settle(pool: &PgPool) {
    if let Err(e) = sqlx::query("checkpoint").execute(pool).await {
        eprintln!(
            "warning: could not checkpoint before the run ({e}); results may depend on \
             what ran before them"
        );
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
}

/// A hook for **benchmark-only** database tuning: if `benchmarks/tuning.sql`
/// exists it is applied and hashed into the result.
///
/// Normally it does not exist, and that is the point. The per-table
/// autovacuum settings used to live there, which was the wrong side of the
/// line — a setting only the benchmark applies makes the benchmark measure a
/// system nobody runs. They are now in the engine's own migration 0009, and
/// what the result records is `postgres.table_options`: the settings that
/// are actually in force, read back from the catalogue, rather than a script
/// asserting what they ought to be.
pub(crate) async fn apply_tuning(pool: &PgPool, root: &Path) -> Result<Option<String>, String> {
    use sha2::{Digest, Sha256};
    let path = root.join("tuning.sql");
    let Ok(sql) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    sqlx::raw_sql(&sql)
        .execute(pool)
        .await
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(Some(format!("{:x}", Sha256::digest(sql.as_bytes()))))
}

/// The monitor, in its own process. Not a task: a sampler competing for the
/// same runtime as the load it samples reports the runtime's scheduling
/// delays as the database's latency.
struct Monitor {
    child: tokio::process::Child,
    output: PathBuf,
}

impl Monitor {
    fn spawn(database_url: &str, interval: f64) -> Result<Monitor, String> {
        let output = std::env::temp_dir().join(format!(
            "rbpmn-bench-monitor-{}.jsonl",
            uuid::Uuid::new_v4().simple()
        ));
        let exe = std::env::current_exe().map_err(|e| format!("locating this binary: {e}"))?;
        let child = tokio::process::Command::new(exe)
            .arg("monitor")
            .arg("--interval")
            .arg(interval.to_string())
            .arg("--out")
            .arg(&output)
            .env("RBPMN_BENCH_DATABASE_URL", database_url)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("spawning the monitor process: {e}"))?;
        Ok(Monitor { child, output })
    }

    async fn finish(mut self) -> Result<Vec<pg::Sample>, String> {
        // SIGKILL would be fine — the monitor appends a line per sample and
        // never buffers a partial one — but a clean stop keeps the last
        // sample's write from racing the read.
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        let text = std::fs::read_to_string(&self.output).unwrap_or_default();
        let _ = std::fs::remove_file(&self.output);
        let mut samples = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            match serde_json::from_str(line) {
                Ok(sample) => samples.push(sample),
                Err(e) => return Err(format!("monitor wrote a line that does not parse: {e}")),
            }
        }
        Ok(samples)
    }
}

/// Host part of a Postgres URL, for the local/remote determination. Parsing
/// rather than string surgery: a URL carrying a password with an `@` in it
/// would otherwise report the wrong host.
pub fn host_of(url: &str) -> String {
    use std::str::FromStr;
    sqlx::postgres::PgConnectOptions::from_str(url)
        .map(|options| options.get_host().to_string())
        .unwrap_or_default()
}

/// Never print a URL with its password in it — result files are committed.
pub fn redact(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => match rest.split_once('@') {
            Some((_, host)) => format!("{scheme}://<credentials>@{host}"),
            None => url.to_string(),
        },
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_never_reach_a_result_file() {
        assert_eq!(
            redact("postgres://rbpmn:hunter2@localhost:55432/rbpmn_bench"),
            "postgres://<credentials>@localhost:55432/rbpmn_bench"
        );
        assert_eq!(redact("postgres://localhost/db"), "postgres://localhost/db");
    }

    #[test]
    fn nearest_rank_percentiles_pick_real_observations() {
        let sorted: Vec<f64> = (1..=100).map(|n| n as f64).collect();
        let latency = Latency::from_sorted(&sorted);
        assert_eq!(latency.p50, 50.0);
        assert_eq!(latency.p95, 95.0);
        assert_eq!(latency.p99, 99.0);
        assert_eq!(latency.max, 100.0);
        assert_eq!(latency.min, 1.0);
    }

    #[test]
    fn an_empty_sample_is_zero_not_a_panic() {
        let latency = Latency::from_sorted(&[]);
        assert_eq!(latency.count, 0);
    }
}
