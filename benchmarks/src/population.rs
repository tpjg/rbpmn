//! Population-scale measurement: standing cost, not throughput.
//!
//! Every other scenario in this suite measures a **rate** — how fast can
//! instances be pushed through. This one measures what everything still
//! costs while a large cohort sits parked doing nothing, because that is the
//! shape of a long-running deployment: year-long flows, a full year's cohort
//! alive at once, and a throughput so low it is not the risk. Two million
//! instances a year is 0.06 per second. The population is 2 000 000.
//!
//! A rate benchmark says nothing about this. A drain that finishes in two
//! seconds never had a million rows to walk past, and every index it used
//! was small enough to be irrelevant.
//!
//! The output is a **curve, not a point**. Each probe runs at every
//! configured size, so the question it answers is not "is this fast" but
//! "does this grow with the population" — the only one that matters when the
//! population is going to be a million either way. A probe that is flat from
//! 10 000 to 1 000 000 is a probe you never have to think about again; one
//! that tracks the population is a design problem, and it is much cheaper to
//! find here than in the second year of production.

use crate::env::{Checkout, Hardware};
use crate::model::Model;
use crate::pg;
use crate::result::Latency;
use crate::scenario::{ParksOn, Scenario};
use crate::vars;
use rbpmn_engine::{Engine, GetTaskOptions};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const SCHEMA: &str = "rbpmn-bench-population/1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PopulationReport {
    pub schema: String,
    pub scenario: String,
    pub run_id: String,
    pub started_at: String,
    pub checkout: Checkout,
    pub hardware: Hardware,
    pub postgres: pg::PostgresFacts,
    pub provenance: crate::result::Provenance,
    pub scope: crate::result::ScopeNotes,
    pub samples_per_probe: u32,
    /// One entry per configured population size, ascending.
    pub steps: Vec<PopulationStep>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PopulationStep {
    /// Instances parked at this step.
    pub population: u32,
    /// How long it took to build *up to* this size from the previous one,
    /// and at what rate. Reported because "how long does it take to park a
    /// million" is a fair operational question, not because it is a headline.
    pub build_secs: f64,
    pub build_instances_per_sec: f64,
    /// Rows and bytes, per table, at this population.
    pub tables: BTreeMap<String, TableFootprint>,
    pub total_bytes: i64,
    pub bytes_per_instance: f64,
    /// Probe name → latency distribution in milliseconds.
    pub probes: BTreeMap<String, Latency>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableFootprint {
    pub rows: i64,
    pub total_bytes: i64,
    pub index_bytes: i64,
}

pub struct PopulationOptions {
    pub root: PathBuf,
    pub database_url: String,
    pub provisioned_by: String,
    /// Overrides the scenario's size ladder (smoke runs).
    pub sizes: Option<Vec<u32>>,
    pub samples: Option<u32>,
    /// Keep an existing population instead of starting from empty. Building a
    /// million takes a while; re-probing one you already have should not.
    pub reuse: bool,
}

pub async fn run(
    scenario: &Scenario,
    options: &PopulationOptions,
) -> Result<PopulationReport, String> {
    let population = scenario.population.as_ref().ok_or_else(|| {
        format!(
            "{} is not a population scenario — it has no [population] section",
            scenario.name
        )
    })?;
    if let Some(reason) = scenario.workload.history.unsupported_reason() {
        return Err(reason);
    }

    let model = Model::load(&scenario.model_path(&options.root))?;
    let bindings = scenario.bindings(&model);
    let warnings_from_check = crate::model::check(&model, &bindings)?;
    check_model_parks_as_declared(&model, population.parks_on, &scenario.name)?;

    let mut sizes = options
        .sizes
        .clone()
        .unwrap_or_else(|| population.sizes.clone());
    sizes.sort_unstable();
    sizes.dedup();
    if sizes.is_empty() {
        return Err(format!("{}: no population sizes configured", scenario.name));
    }
    let samples = options.samples.unwrap_or(population.samples);
    let run_id = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
    let started_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(scenario.execute.db_pool)
        .acquire_timeout(Duration::from_secs(60))
        .connect(&options.database_url)
        .await
        .map_err(|e| {
            format!(
                "connecting to {}: {e}",
                crate::run::redact(&options.database_url)
            )
        })?;

    // Handlers are registered so `deploy` passes `unresolved-topic`; no
    // worker loop ever runs them, because the build completes work items by
    // id (see `build_to`).
    let mut builder = Engine::builder(pool.clone());
    for topic in scenario.service_topics(&model) {
        builder = builder.declare_topic(topic);
    }
    let engine = builder.build();
    engine
        .migrate()
        .await
        .map_err(|e| format!("migrate: {e}"))?;
    if !options.reuse {
        crate::run::truncate_all(&pool).await?;
    }
    let tuning = crate::run::apply_tuning(&pool, &options.root).await?;
    engine
        .sync_environment()
        .await
        .map_err(|e| format!("sync_environment: {e}"))?;
    engine
        .deploy(&model.xml, &bindings)
        .await
        .map_err(|e| format!("deploy {}: {e}", model.file))?;

    let facts = pg::facts(
        &pool,
        &crate::run::host_of(&options.database_url),
        &options.provisioned_by,
    )
    .await
    .map_err(|e| format!("capturing postgres settings: {e}"))?;
    let hardware = Hardware::detect(&options.root);
    let checkout = Checkout::detect(&options.root);

    let mut warnings: Vec<String> = warnings_from_check
        .iter()
        .map(|w| format!("lint: {w}"))
        .collect();
    warnings.extend(hardware.declaration_mismatches.iter().cloned());
    if checkout.dirty {
        warnings.push("the checkout has uncommitted changes".to_string());
    }
    if options.reuse {
        warnings.push(
            "--reuse: the population was not rebuilt, so its instances may come from an \
             earlier run with different seeding. Build sizes and rates below describe \
             only what this run added."
                .to_string(),
        );
    }

    // The workers only exist to advance instances from `start` to the wait
    // state. They are stopped before any probe runs — a probe competing with
    // a worker pool would be measuring the pool.
    let mut steps = Vec::new();
    let mut parked_so_far = parked_count(&pool, &model.key).await?;
    for &size in &sizes {
        let build = build_to(
            &engine,
            &pool,
            scenario,
            &model,
            &run_id,
            population.builders,
            parked_so_far,
            size,
        )
        .await?;
        parked_so_far = size.max(parked_so_far);

        // Settle the database before probing, in both senses.
        //
        // ANALYZE, because every probe below is a plan the planner has to
        // choose and the point is to measure the engine rather than when
        // autovacuum last ran (see `run::analyze_before_execute`).
        //
        // VACUUM, because building the population *completed* one work item
        // per instance, and completion removes the row from the partial
        // claim indexes — leaving a dead entry behind until vacuum reclaims
        // it. Until then a claim walks past all of them. Measured at a
        // million: the same claim took 25.885 ms with the build's dead
        // entries still in the index and 0.041 ms straight after a VACUUM.
        // 630x, and it is what made two runs of this ladder disagree by up
        // to 14x on the claim probes with identical code. Probing without
        // this measures how far behind autovacuum happened to be.
        //
        // It is not hiding a real cost — vacuum lag after heavy completion
        // churn is a genuine operational concern, and the README says so —
        // but it is a *different* measurement from standing cost at rest,
        // and mixing the two produces a number that answers neither.
        sqlx::query("vacuum analyze rbpmn_work_item, rbpmn_instance, rbpmn_timer")
            .execute(&pool)
            .await
            .map_err(|e| format!("vacuuming at population {size}: {e}"))?;
        sqlx::query("analyze")
            .execute(&pool)
            .await
            .map_err(|e| format!("analyzing at population {size}: {e}"))?;

        let probes = probe_all(&engine, &pool, scenario, &model, &run_id, samples).await?;
        // Counted, not assumed: the probes add their own instances to the
        // cohort (see `claim_hit`), and a step that reported the size it
        // *asked* for would be describing a population that is not there.
        parked_so_far = parked_count(&pool, &model.key).await?.max(parked_so_far);
        let (tables, total_bytes) = footprint(&pool).await?;
        steps.push(PopulationStep {
            population: parked_so_far,
            build_secs: build.secs,
            build_instances_per_sec: build.rate,
            bytes_per_instance: if parked_so_far == 0 {
                0.0
            } else {
                total_bytes as f64 / f64::from(parked_so_far)
            },
            tables,
            total_bytes,
            probes,
        });
    }

    let mut models = BTreeMap::new();
    models.insert(model.file.clone(), model.sha256.clone());

    Ok(PopulationReport {
        schema: SCHEMA.to_string(),
        scenario: scenario.name.clone(),
        run_id,
        started_at,
        checkout,
        hardware,
        postgres: facts,
        provenance: crate::result::Provenance {
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
        scope: crate::result::ScopeNotes::new(scenario, &model),
        samples_per_probe: samples,
        steps,
        warnings,
    })
}

/// The scenario declares what its cohort parks on; the model decides. A
/// mismatch would leave a probe measuring an empty table and reporting it as
/// a fast one.
fn check_model_parks_as_declared(
    model: &Model,
    parks_on: ParksOn,
    name: &str,
) -> Result<(), String> {
    match parks_on {
        ParksOn::Timer if model.timers == 0 => Err(format!(
            "{name}: [population] parks_on = \"timer\" but {} has no timer",
            model.file
        )),
        ParksOn::Message if model.message_catches == 0 => Err(format!(
            "{name}: [population] parks_on = \"message\" but {} has no message catch",
            model.file
        )),
        _ => Ok(()),
    }
}

struct Build {
    secs: f64,
    rate: f64,
}

/// Top the population up to `target`, then stop every worker.
///
/// Parallel on purpose: the build is setup, not measurement. What is
/// measured is what happens *after* it, with nothing else running.
#[allow(clippy::too_many_arguments)]
async fn build_to(
    engine: &Engine,
    pool: &PgPool,
    scenario: &Scenario,
    model: &Model,
    run_id: &str,
    builders: u32,
    from: u32,
    target: u32,
) -> Result<Build, String> {
    if target <= from {
        return Ok(Build {
            secs: 0.0,
            rate: 0.0,
        });
    }
    let started = Instant::now();

    // Advance each instance from its start event to the wait state by
    // completing its work item **by id**, not by claiming it by topic.
    //
    // That is deliberate and it is not the engine's normal path. The push
    // worker claims with `topic = any($1)`, which cannot use the claim
    // index's ordering and therefore sorts the entire claimable backlog to
    // take one row (see the README's findings). Building a population *is* a
    // large backlog by construction, so a build that went through the push
    // worker would spend its time on that and never reach a million — it
    // measured ~30 ms per claim at 100 000 and was spilling sort buffers to
    // disk. Completion by id runs the identical transactional step; only the
    // choosing of which item is different, and the build is setup rather
    // than measurement. It also keeps the build honest: the build rate below
    // is not a claim-path number and must not be read as one.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut workers = Vec::new();
    for _ in 0..scenario.execute.service_workers.max(1) {
        let engine = engine.clone();
        let pool = pool.clone();
        let stop = stop.clone();
        let patch = scenario
            .execute
            .patch
            .clone()
            .unwrap_or_else(|| serde_json::json!({ "registered": true }));
        workers.push(tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                let ids: Vec<Uuid> = match sqlx::query_scalar(
                    "select id from rbpmn_work_item where state = 'available' limit 64",
                )
                .fetch_all(&pool)
                .await
                {
                    Ok(ids) => ids,
                    Err(_) => {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        continue;
                    }
                };
                if ids.is_empty() {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                }
                for id in ids {
                    // Two builders racing for one item is normal; the loser
                    // gets the engine's idempotent already-closed no-op.
                    let _ = engine.complete_work_item(id, patch.clone()).await;
                }
            }
        }));
    }

    let next = Arc::new(AtomicU32::new(from));
    let mut starters = Vec::new();
    for _ in 0..builders.max(1) {
        let engine = engine.clone();
        let next = next.clone();
        let fields = scenario.workload.fields.clone();
        let seed = scenario.workload.seed;
        let run_id = run_id.to_string();
        let key = model.key.clone();
        starters.push(tokio::spawn(async move {
            loop {
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= target {
                    return Ok::<(), String>(());
                }
                let variables = vars::document(&fields, seed, &run_id, index);
                engine
                    .start(&key, Some(&format!("pop:{index}")), variables)
                    .await
                    .map_err(|e| format!("start #{index}: {e}"))?;
            }
        }));
    }
    for starter in starters {
        starter
            .await
            .map_err(|e| format!("a builder task panicked: {e}"))??;
    }

    // Every start has been issued; the workers are still advancing those
    // instances to the wait state. Refresh statistics before they do the
    // bulk of it — the population just grew by an order of magnitude, and a
    // claim planned against the previous size flips to the plan documented
    // in `run::analyze_before_execute`. Cheap here, and the build is setup.
    sqlx::query("analyze rbpmn_instance, rbpmn_work_item")
        .execute(pool)
        .await
        .map_err(|e| format!("analyzing mid-build: {e}"))?;

    // Wait until nothing is left to claim, which means every started
    // instance has reached its wait state.
    //
    // An existence check on the partial claim index, not a count of parked
    // instances: the obvious version — active instances with no open work
    // item — is an anti-join over the whole population, and at 100 000 it
    // measured 71 ms while running every 100 ms. A harness that polls in
    // O(population) becomes the load it is trying to observe. Third time
    // this exact mistake has appeared in this file's history; the rule is
    // that anything in a loop must cost what is *left to do*, never what
    // has been done.
    let deadline = Instant::now() + Duration::from_secs(3600);
    loop {
        let open: bool = sqlx::query_scalar(
            "select exists (select 1 from rbpmn_work_item \
             where state in ('available','locked'))",
        )
        .fetch_one(pool)
        .await
        .map_err(|e| format!("polling build progress: {e}"))?;
        if !open {
            break;
        }
        if Instant::now() > deadline {
            return Err(format!("building to {target} stalled with work still open"));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    stop.store(true, Ordering::Relaxed);
    for worker in workers {
        worker.abort();
    }

    let secs = started.elapsed().as_secs_f64();
    Ok(Build {
        secs,
        rate: f64::from(target - from) / secs.max(f64::EPSILON),
    })
}

/// Instances of this definition that have reached the wait state: active,
/// with no open work item. Indexed on `(definition_key, status)`.
async fn parked_count(pool: &PgPool, definition_key: &str) -> Result<u32, String> {
    let count: i64 = sqlx::query_scalar(
        "select count(*) from rbpmn_instance i \
         where i.definition_key = $1 and i.status = 'active' \
           and not exists (select 1 from rbpmn_work_item w \
                where w.instance_id = i.id and w.state in ('available','locked'))",
    )
    .bind(definition_key)
    .fetch_one(pool)
    .await
    .map_err(|e| format!("counting parked instances: {e}"))?;
    Ok(count as u32)
}

/// Time `samples` repetitions of `probe`, returning the millisecond
/// distribution. Individually timed, so the answer is a distribution and not
/// a mean — the tail is where a plan that degrades with the population shows
/// itself first.
async fn timed<F, Fut>(samples: u32, mut probe: F) -> Result<Latency, String>
where
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let mut millis = Vec::with_capacity(samples as usize);
    for i in 0..samples {
        let started = Instant::now();
        probe(i).await?;
        millis.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    millis.sort_by(|a, b| a.partial_cmp(b).expect("finite"));
    Ok(Latency::from_sorted(&millis))
}

async fn probe_all(
    engine: &Engine,
    pool: &PgPool,
    scenario: &Scenario,
    model: &Model,
    run_id: &str,
    samples: u32,
) -> Result<BTreeMap<String, Latency>, String> {
    let mut probes = BTreeMap::new();
    let topic = &scenario.bindings.service_topic;

    // The single most-executed query in a quiet system: a worker asking for
    // work and being told there is none. If anything here tracks the
    // population, every idle worker pays it forever.
    //
    // A short lease on purpose. The first version left the default ten
    // minutes and never completed what `claim_hit` claimed, so the *next*
    // build phase sat waiting for those leases to expire — it read as a
    // build running at 13 instances/sec instead of 1500. Probes must leave
    // the population exactly as they found it; see `clean_up_probes`.
    let mut options = GetTaskOptions::new("probe");
    options.ttl = Duration::from_secs(5);
    probes.insert(
        "claim_empty".to_string(),
        timed(samples, |_| async {
            engine
                .get_task(topic, &options)
                .await
                .map(|_| ())
                .map_err(|e| format!("claim probe: {e}"))
        })
        .await?,
    );

    probes.insert(
        "count_tasks".to_string(),
        timed(samples, |_| async {
            engine
                .count_tasks(topic, None)
                .await
                .map(|_| ())
                .map_err(|e| format!("count probe: {e}"))
        })
        .await?,
    );

    // Starting an instance into a table that already holds a million.
    let start_base = 900_000_000u32;
    probes.insert(
        "start_instance".to_string(),
        timed(samples, |i| {
            let variables = vars::document(
                &scenario.workload.fields,
                scenario.workload.seed,
                run_id,
                start_base + i,
            );
            async move {
                engine
                    .start(&model.key, Some("pop:probe"), variables)
                    .await
                    .map(|_| ())
                    .map_err(|e| format!("start probe: {e}"))
            }
        })
        .await?,
    );

    // Those probe instances left claimable work items behind — claim them,
    // which is the same query as `claim_empty` with a row to find. Each
    // claim is completed immediately, which does two things: it measures a
    // whole claim-and-complete rather than half of one, and it advances the
    // probe's instances to the wait state so they **join the population**
    // instead of lingering as instances that can never park.
    //
    // Joining is the right resolution and the earlier ones were not. Not
    // completing them wedged the next build for a full lease TTL; deleting
    // them afterwards is refused by `delete_instance`, correctly — it only
    // removes terminal instances, and an escape hatch that deleted live ones
    // would not be an escape hatch. So the population is re-counted after
    // probing rather than assumed, and it is a few hundred larger than the
    // ladder asked for. That is recorded, not hidden.
    probes.insert(
        "claim_hit".to_string(),
        timed(samples, |_| async {
            match engine
                .get_task(topic, &options)
                .await
                .map_err(|e| format!("claim-hit probe: {e}"))?
            {
                Some(task) => engine
                    .complete_task(task.id, "probe", serde_json::json!({ "probed": true }))
                    .await
                    .map(|_| ())
                    .map_err(|e| format!("claim-hit completion: {e}")),
                None => Ok(()),
            }
        })
        .await?,
    );

    // The engine's own `read_events`, not an approximation of it: a probe
    // that hand-wrote the cursor query would be benchmarking the probe.
    probes.insert(
        "event_page".to_string(),
        timed(samples, |_| async {
            engine
                .read_events(rbpmn_engine::EventCursor { txid: 0, id: 0 }, 100)
                .await
                .map(|_| ())
                .map_err(|e| format!("event page probe: {e}"))
        })
        .await?,
    );

    // What startup re-validation and `undeclare_topic` ask: which definitions
    // still have an active instance. Over a million active rows this is the
    // admin path most likely to go from instant to noticeable.
    probes.insert(
        "admin_definitions_in_use".to_string(),
        timed(samples.min(20), |_| async {
            sqlx::query(
                "select distinct d.key from rbpmn_definition d \
                 where d.id in (select definition_id from rbpmn_instance where status = 'active')",
            )
            .fetch_all(pool)
            .await
            .map(|_| ())
            .map_err(|e| format!("admin probe: {e}"))
        })
        .await?,
    );

    let ids = sample_instance_ids(pool, &model.key, samples).await?;
    if !ids.is_empty() {
        probes.insert(
            "inspect_instance".to_string(),
            timed(samples, |i| {
                let id = ids[i as usize % ids.len()];
                async move {
                    engine
                        .inspect_instance(id)
                        .await
                        .map(|_| ())
                        .map_err(|e| format!("inspect probe: {e}"))
                }
            })
            .await?,
        );
    }

    match scenario
        .population
        .as_ref()
        .map(|p| p.parks_on)
        .unwrap_or(ParksOn::Timer)
    {
        ParksOn::Timer => {
            // The scheduler's sleep computation, run on every idle cycle of
            // every node: `min(due_at)` over the whole armed set.
            probes.insert(
                "timer_next_due".to_string(),
                timed(samples, |_| async {
                    engine
                        .next_due_in()
                        .await
                        .map(|_| ())
                        .map_err(|e| format!("next-due probe: {e}"))
                })
                .await?,
            );

            // Firing one timer against N armed. Due dates are forced rather
            // than waited for — a year does not elapse during a benchmark —
            // and that is the only shortcut here: the claim, the instance
            // lock, the re-check, the step and the row delete are all real.
            // Divide N by this rate for the storm estimate.
            let forced = force_due(pool, &model.key, samples).await?;
            if forced > 0 {
                probes.insert(
                    "timer_fire".to_string(),
                    timed(forced, |_| async {
                        engine
                            .fire_due_timer()
                            .await
                            .map(|_| ())
                            .map_err(|e| format!("timer-fire probe: {e}"))
                    })
                    .await?,
                );
            }
        }
        ParksOn::Message => {
            // The one this model exists for: exactly-one delivery among a
            // million open subscriptions.
            let correlation = scenario
                .bindings
                .correlation
                .as_ref()
                .ok_or("population-message needs a bound correlation")?;
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
            probes.insert(
                "correlate".to_string(),
                timed(samples, |i| {
                    let key = vars::unique(&prefix, run_id, i);
                    async move {
                        match engine
                            .correlate(&correlation.message, &key, serde_json::json!({}))
                            .await
                        {
                            Ok(_) => Ok(()),
                            // A key whose instance this run did not build.
                            // The lookup — the thing being measured — still
                            // happened against the whole index.
                            Err(rbpmn_engine::EngineError::NoSubscription { .. }) => Ok(()),
                            Err(e) => Err(format!("correlate probe: {e}")),
                        }
                    }
                })
                .await?,
            );
        }
    }

    Ok(probes)
}

/// A spread of instance ids to inspect — `order by random()` would itself be
/// a population-sized sort, so this walks the primary key instead.
async fn sample_instance_ids(
    pool: &PgPool,
    definition_key: &str,
    limit: u32,
) -> Result<Vec<Uuid>, String> {
    let rows = sqlx::query(
        "select id from rbpmn_instance where definition_key = $1 and status = 'active' \
         limit $2",
    )
    .bind(definition_key)
    .bind(i64::from(limit))
    .fetch_all(pool)
    .await
    .map_err(|e| format!("sampling instance ids: {e}"))?;
    Ok(rows.iter().map(|row| row.get::<Uuid, _>("id")).collect())
}

/// Make `count` armed timers due now, so `fire_due_timer` has something to
/// claim. Reaching into the timer table is deliberate and is the only place
/// the harness does so: the alternative is waiting a year.
async fn force_due(pool: &PgPool, definition_key: &str, count: u32) -> Result<u32, String> {
    let updated = sqlx::query(
        "update rbpmn_timer set due_at = now() - interval '1 second' \
         where (instance_id, timer_no) in ( \
            select t.instance_id, t.timer_no from rbpmn_timer t \
            join rbpmn_instance i on i.id = t.instance_id \
            where i.definition_key = $1 and i.status = 'active' limit $2)",
    )
    .bind(definition_key)
    .bind(i64::from(count))
    .execute(pool)
    .await
    .map_err(|e| format!("forcing timers due: {e}"))?;
    Ok(updated.rows_affected() as u32)
}

async fn footprint(pool: &PgPool) -> Result<(BTreeMap<String, TableFootprint>, i64), String> {
    let rows = sqlx::query(
        "select relname, n_live_tup as rows, \
                pg_total_relation_size(relid) as total_bytes, \
                pg_indexes_size(relid) as index_bytes \
         from pg_stat_user_tables where relname like 'rbpmn\\_%' order by relname",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| format!("measuring the footprint: {e}"))?;
    let mut tables = BTreeMap::new();
    let mut total = 0i64;
    for row in rows {
        let bytes: i64 = row.get("total_bytes");
        total += bytes;
        tables.insert(
            row.get::<String, _>("relname"),
            TableFootprint {
                rows: row.get("rows"),
                total_bytes: bytes,
                index_bytes: row.get("index_bytes"),
            },
        );
    }
    Ok((tables, total))
}

impl PopulationReport {
    pub fn write(&self, root: &Path) -> Result<PathBuf, String> {
        let date = self.started_at.get(..10).unwrap_or("unknown-date");
        // The scenario name already begins with `population-`; prefixing it
        // again produced `population-population-timer-…`.
        let path = root.join("results").join(format!(
            "{}-{date}-{}.json",
            self.scenario, self.hardware.host_id
        ));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, format!("{json}\n"))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(path)
    }

    /// The curve, as a table: every probe's p50 at every population size.
    /// Read across a row — flat means the population does not matter for
    /// that path, and a rising row is the finding.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let mut names: Vec<&String> = self
            .steps
            .iter()
            .flat_map(|s| s.probes.keys())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        names.sort();

        out.push_str(&format!("{:<26}", "probe p50 (ms)"));
        for step in &self.steps {
            out.push_str(&format!("{:>14}", thousands(step.population)));
        }
        out.push_str("     growth\n");

        for name in names {
            out.push_str(&format!("{name:<26}"));
            let mut first = None;
            let mut last = None;
            for step in &self.steps {
                match step.probes.get(name) {
                    Some(latency) => {
                        first.get_or_insert(latency.p50);
                        last = Some(latency.p50);
                        out.push_str(&format!("{:>14.3}", latency.p50));
                    }
                    None => out.push_str(&format!("{:>14}", "—")),
                }
            }
            match (first, last) {
                (Some(a), Some(b)) if a > 0.0 => out.push_str(&format!("   {:>6.1}x", b / a)),
                _ => out.push_str("        —"),
            }
            out.push('\n');
        }

        out.push_str(&format!("\n{:<26}", "population"));
        for step in &self.steps {
            out.push_str(&format!("{:>14}", thousands(step.population)));
        }
        out.push_str(&format!("\n{:<26}", "total bytes"));
        for step in &self.steps {
            out.push_str(&format!("{:>14}", bytes(step.total_bytes)));
        }
        out.push_str(&format!("\n{:<26}", "bytes/instance"));
        for step in &self.steps {
            out.push_str(&format!("{:>14.0}", step.bytes_per_instance));
        }
        out.push_str(&format!("\n{:<26}", "build (instances/sec)"));
        for step in &self.steps {
            out.push_str(&format!("{:>14.0}", step.build_instances_per_sec));
        }
        out.push('\n');
        out
    }
}

fn thousands(n: u32) -> String {
    let text = n.to_string();
    let mut out = String::new();
    for (i, c) in text.chars().enumerate() {
        if i > 0 && (text.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

fn bytes(n: i64) -> String {
    const UNITS: [&str; 4] = ["B", "kB", "MB", "GB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_render_readably() {
        assert_eq!(thousands(1_000_000), "1 000 000");
        assert_eq!(thousands(10_000), "10 000");
        assert_eq!(thousands(7), "7");
    }

    #[test]
    fn byte_sizes_render_readably() {
        assert_eq!(bytes(512), "512.0 B");
        assert_eq!(bytes(1024 * 1024), "1.0 MB");
    }
}
