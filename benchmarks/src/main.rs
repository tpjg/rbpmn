//! `rbpmn-bench` — the benchmark harness.
//!
//! A separate track from the correctness tests, and it stays separate: no
//! benchmark ever gates CI on an absolute number, because absolute numbers
//! belong to a machine. The one thing allowed to fail a build is the
//! pure-core micro suite compared against a baseline recorded **on that same
//! machine** (`gate`).
//!
//! Everything the harness knows about a run — the checkout, the hardware,
//! every Postgres setting, the seed, the model and scenario hashes — lands
//! in the result file, because a benchmark number without its conditions is
//! folklore. `benchmarks/README.md` says what each scenario does and does
//! not measure; the same prose is copied into every result file so a number
//! that outlives its documentation still carries it.

#![forbid(unsafe_code)]

mod env;
mod gate;
mod micro;
mod model;
mod monitor;
mod pg;
mod population;
mod report;
mod result;
mod run;
mod scenario;
mod vars;

use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

/// Where the benchmark database lives when nobody says otherwise: the local
/// Postgres this repository already assumes for `just serve` and the engine's
/// integration tests. **No Docker is required to run these benchmarks** —
/// `benchmarks/compose.yml` exists for people who want a pinned, tuned server
/// rather than whatever their machine happens to run, and it is opt-in
/// (`just bench-compose`). Either way the result file records which one it
/// was, and every setting it ran under.
fn default_url() -> String {
    let user = std::env::var("PGUSER")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "postgres".into());
    format!("postgres://{user}@localhost:5432/rbpmn_bench")
}

/// The harness writes hundreds of thousands of rows and rewrites per-table
/// autovacuum settings. Both are fine on a database that exists to be
/// benchmarked and catastrophic on one that does not, and the difference is
/// not something a URL makes obvious at a glance. So: the name has to say
/// so, or the caller has to say so.
fn guard_database(url: &str, allow_any: bool) -> Result<(), String> {
    if allow_any {
        return Ok(());
    }
    let name = database_name(url);
    if name.to_ascii_lowercase().contains("bench") {
        return Ok(());
    }
    Err(format!(
        "refusing to benchmark database '{name}': the harness starts hundreds of \
         thousands of instances and applies per-table autovacuum settings \
         (benchmarks/tuning.sql), so it only runs against a database whose name says \
         it is for benchmarking. Create one — `createdb rbpmn_bench` — or pass \
         --allow-any-database if you are sure."
    ))
}

fn database_name(url: &str) -> String {
    use std::str::FromStr;
    sqlx::postgres::PgConnectOptions::from_str(url)
        .ok()
        .and_then(|options| options.get_database().map(|db| db.to_string()))
        .unwrap_or_default()
}

#[derive(Parser)]
#[command(
    name = "rbpmn-bench",
    about = "rbpmn benchmarks: models, data, hardware spec, one command",
    disable_help_subcommand = true
)]
struct Cli {
    /// The `benchmarks/` directory. Defaults to the one this binary was
    /// built from, so the harness works from any working directory.
    #[arg(long, global = true)]
    root: Option<PathBuf>,
    /// Postgres URL. Also read from `RBPMN_BENCH_DATABASE_URL`. Defaults to
    /// the local server (`postgres://$USER@localhost:5432/rbpmn_bench`) — no
    /// Docker needed.
    #[arg(long, global = true)]
    database_url: Option<String>,
    /// Run against a database whose name does not say it is for
    /// benchmarking. See `guard_database`.
    #[arg(long, global = true)]
    allow_any_database: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Lint and compile every scenario's model against its manifest. No
    /// database, no Docker — the fast check that a benchmark model is still
    /// a model this engine would deploy.
    Check,
    /// List the scenarios and what they measure.
    List,
    /// Warm up, park a backlog, drain it, measure, write a result file.
    Run(RunArgs),
    /// Park a backlog and stop (workers stay off). Pairs with `execute`.
    Generate(RunArgs),
    /// Drain and measure a backlog `generate` parked.
    Execute(RunArgs),
    /// Sample the database while a run is in flight. Its own process on
    /// purpose — see `src/monitor.rs`.
    Monitor {
        #[arg(long, default_value = "1.0")]
        interval: f64,
        /// JSONL output. Defaults to stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Population-scale measurement: park a large cohort, then probe what
    /// everything still costs at rest. Standing cost, not throughput — the
    /// question a long-running deployment actually has.
    Population {
        /// Scenario name (a `[population]` one).
        scenario: String,
        /// Override the size ladder, e.g. `1000,10000`.
        #[arg(long, value_delimiter = ',')]
        sizes: Option<Vec<u32>>,
        #[arg(long)]
        samples: Option<u32>,
        /// Probe an existing population instead of rebuilding it.
        #[arg(long)]
        reuse: bool,
    },
    /// The persisted pattern micro-benchmarks: per-construct cost including
    /// the rows it writes. Reported, never gated.
    MicroPersisted {
        #[arg(long, default_value = "200")]
        iterations: u32,
    },
    /// Compare the pure-core micro suite against this machine's committed
    /// baseline. The only command that can fail a build.
    Gate {
        #[arg(long)]
        criterion_dir: PathBuf,
        /// Fractional slowdown tolerated before a benchmark is a regression.
        #[arg(long, default_value = "0.25")]
        threshold: f64,
    },
    /// Record the pure-core baseline for this machine, into the gitignored
    /// `benchmarks/.baselines/`. Explicit and manual — a baseline that
    /// re-recorded itself would ratchet a regression in.
    RecordBaseline {
        #[arg(long)]
        criterion_dir: PathBuf,
    },
    /// Render the committed results into a markdown comparison table.
    Report {
        /// Write here instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(clap::Args)]
struct RunArgs {
    /// Scenario name (file stem in `scenarios/`). Omit with `--all`.
    scenario: Option<String>,
    /// Every scenario, in order.
    #[arg(long)]
    all: bool,
    /// `saturation` (drain a backlog — throughput) or `steady` (open-loop
    /// arrivals — latency under load).
    #[arg(long, default_value = "saturation")]
    mode: String,
    #[arg(long)]
    instances: Option<u32>,
    #[arg(long)]
    warmup: Option<u32>,
    /// Monitor sampling interval in seconds. `0` switches the monitor off.
    #[arg(long, default_value = "1.0")]
    monitor_interval: f64,
    /// Abort when nothing completes for this many seconds.
    #[arg(long, default_value = "60")]
    stall_timeout: u64,
    /// Reuse the run id of an earlier `generate`.
    #[arg(long)]
    run_id: Option<String>,
    /// Keep whatever the database already holds instead of emptying it
    /// first. Runs stop being comparable — and the claim path's plan starts
    /// depending on how much closed history is sitting there.
    #[arg(long)]
    no_fresh: bool,
    /// Skip the `ANALYZE` between parking the backlog and draining it. This
    /// reproduces the stale-statistics plan flip documented in
    /// `run::analyze_before_execute` (~8x on mixed-typical); it is not a
    /// faster way to run the benchmark.
    #[arg(long)]
    no_analyze: bool,
    /// How this database got here, recorded in the result file. Defaults to
    /// `local` for the machine's own Postgres, `external` when a URL was
    /// supplied; `just bench-compose` passes `compose`.
    #[arg(long)]
    provisioned_by: Option<String>,
}

impl RunArgs {
    fn provisioned_by(&self, supplied_url: bool) -> String {
        self.provisioned_by
            .clone()
            .unwrap_or_else(|| if supplied_url { "external" } else { "local" }.to_string())
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let root = cli
        .root
        .clone()
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
    let supplied_url = cli.database_url.is_some()
        || std::env::var("RBPMN_BENCH_DATABASE_URL").is_ok_and(|url| !url.is_empty());
    let database_url = cli
        .database_url
        .clone()
        .or_else(|| std::env::var("RBPMN_BENCH_DATABASE_URL").ok())
        .filter(|url| !url.is_empty())
        .unwrap_or_else(default_url);

    // Every command that writes touches the database guard first. `check`,
    // `list`, `gate`, `record-baseline` and `report` never connect at all.
    if matches!(
        cli.command,
        Command::Run(_)
            | Command::Generate(_)
            | Command::Execute(_)
            | Command::MicroPersisted { .. }
    ) && let Err(e) = guard_database(&database_url, cli.allow_any_database)
    {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }

    let outcome = match &cli.command {
        Command::Check => check(&root),
        Command::List => list(&root),
        Command::Run(args) => block_on(lifecycle(
            &root,
            &database_url,
            args,
            run::Phases::Both,
            supplied_url,
        )),
        Command::Generate(args) => block_on(lifecycle(
            &root,
            &database_url,
            args,
            run::Phases::Generate,
            supplied_url,
        )),
        Command::Execute(args) => block_on(lifecycle(
            &root,
            &database_url,
            args,
            run::Phases::Execute,
            supplied_url,
        )),
        Command::Monitor { interval, out } => {
            block_on(monitor::run(&database_url, *interval, out.as_deref()))
        }
        Command::Population {
            scenario,
            sizes,
            samples,
            reuse,
        } => block_on(population_run(
            &root,
            &database_url,
            scenario,
            sizes.clone(),
            *samples,
            *reuse,
            if supplied_url { "external" } else { "local" },
        )),
        Command::MicroPersisted { iterations } => block_on(micro_persisted(
            &root,
            &database_url,
            *iterations,
            if supplied_url { "external" } else { "local" },
        )),
        Command::Gate {
            criterion_dir,
            threshold,
        } => return gate_micro(&root, criterion_dir, *threshold),
        Command::RecordBaseline { criterion_dir } => record_baseline(&root, criterion_dir),
        Command::Report { out } => render_report(&root, out.as_deref()),
    };

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

/// One multi-threaded runtime for every async command. Built here rather
/// than via `#[tokio::main]` so the synchronous commands (`check`, `list`,
/// `gate`, `report`) never start one at all.
fn block_on<F: std::future::Future<Output = Result<(), String>>>(future: F) -> Result<(), String> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("building the tokio runtime: {e}"))?
        .block_on(future)
}

fn check(root: &Path) -> Result<(), String> {
    let scenarios = scenario::load_all(root)?;
    if scenarios.is_empty() {
        return Err(format!(
            "no scenarios in {}",
            root.join("scenarios").display()
        ));
    }
    let mut warned = 0;
    for scenario in &scenarios {
        let model = model::Model::load(&scenario.model_path(root))?;
        let bindings = scenario.bindings(&model);
        let diagnostics = model::check(&model, &bindings)?;
        println!(
            "ok  {:<18} {:<24} {} elements, {} service, {} user, {} timers, {} messages, \
             {} subprocesses",
            scenario.name,
            model.file,
            model.elements,
            model.service_tasks.len(),
            model.user_tasks.len(),
            model.timers,
            model.message_catches,
            model.subprocesses,
        );
        for diagnostic in diagnostics {
            warned += 1;
            println!("    warn {diagnostic}");
        }
        // A scenario whose model has no user task but declares user workers
        // (or the reverse) would drain, slowly, while measuring a worker
        // pool that never claims anything. Catch it here, not at 3am.
        if scenario.execute.user_workers > 0 && model.user_tasks.is_empty() {
            return Err(format!(
                "{}: user_workers = {} but the model has no user task",
                scenario.name, scenario.execute.user_workers
            ));
        }
        if !model.user_tasks.is_empty() && scenario.user_topic().is_none() {
            return Err(format!(
                "{}: the model has user tasks but the manifest binds no user_topic — \
                 their topics would default to element ids and no worker would find them",
                scenario.name
            ));
        }
        if scenario.execute.correlators > 0 && scenario.bindings.correlation.is_none() {
            return Err(format!(
                "{}: correlators are configured but no correlation is bound",
                scenario.name
            ));
        }
        // Rate scenarios drain to completion, so every wait state needs
        // something to release it. Population scenarios deliberately do not:
        // their cohort stays parked, and that is the measurement.
        match &scenario.population {
            None => {
                if model.message_catches > 0 && scenario.execute.correlators == 0 {
                    return Err(format!(
                        "{}: the model waits on a message but no correlator would deliver \
                         it — the drain would stall",
                        scenario.name
                    ));
                }
            }
            Some(population) => {
                if population.sizes.is_empty() {
                    return Err(format!("{}: [population] sizes is empty", scenario.name));
                }
                let parks_on_ok = match population.parks_on {
                    scenario::ParksOn::Timer => model.timers > 0,
                    scenario::ParksOn::Message => model.message_catches > 0,
                };
                if !parks_on_ok {
                    return Err(format!(
                        "{}: [population] parks_on = {:?} but {} has no such wait state — \
                         the cohort would never park and every probe would measure an \
                         empty table",
                        scenario.name, population.parks_on, model.file
                    ));
                }
                println!(
                    "    population: parks on {:?}, sizes {:?}, {} samples/probe",
                    population.parks_on, population.sizes, population.samples
                );
            }
        }
    }
    println!(
        "\n{} scenarios check clean ({warned} warnings)",
        scenarios.len()
    );
    Ok(())
}

fn list(root: &Path) -> Result<(), String> {
    for scenario in scenario::load_all(root)? {
        println!("{:<18} {}", scenario.name, scenario.summary);
        for line in &scenario.measures {
            println!("    measures: {line}");
        }
        for line in &scenario.excludes {
            println!("    excludes: {line}");
        }
        println!();
    }
    Ok(())
}

async fn lifecycle(
    root: &Path,
    database_url: &str,
    args: &RunArgs,
    phases: run::Phases,
    supplied_url: bool,
) -> Result<(), String> {
    let mode = match args.mode.as_str() {
        "saturation" => run::Mode::Saturation,
        "steady" => run::Mode::Steady,
        other => return Err(format!("unknown mode '{other}' (saturation | steady)")),
    };
    let scenarios = select(root, args)?;
    for scenario in &scenarios {
        println!("=== {} — {}", scenario.name, scenario.summary);
        let options = run::RunOptions {
            root: root.to_path_buf(),
            database_url: database_url.to_string(),
            provisioned_by: args.provisioned_by(supplied_url),
            mode,
            phases,
            run_id: args.run_id.clone(),
            monitor_interval_secs: (args.monitor_interval > 0.0 && phases == run::Phases::Both)
                .then_some(args.monitor_interval),
            stall_timeout: Duration::from_secs(args.stall_timeout),
            instances: args.instances,
            warmup: args.warmup,
            fresh: !args.no_fresh,
            analyze: !args.no_analyze,
        };
        let Some(result) = run::run(scenario, &options).await? else {
            continue; // `generate` printed what to do next
        };
        let replacing = result.replaces(root);
        let path = result.write(root)?;
        let m = &result.measurements;
        println!(
            "    {} instances in {:.2}s — {:.1} instances/sec\n    \
             latency ({}) p50 {:.1}ms  p95 {:.1}ms  p99 {:.1}ms  max {:.1}ms\n    \
             {} work items, {:.1} events/instance, {:.0} event bytes/instance",
            m.completed_instances,
            m.duration_secs,
            m.throughput_instances_per_sec,
            m.latency_kind,
            m.latency_ms.p50,
            m.latency_ms.p95,
            m.latency_ms.p99,
            m.latency_ms.max,
            m.work_items_completed,
            m.events_per_instance,
            m.event_bytes_per_instance,
        );
        for warning in &result.warnings {
            println!("    warning: {warning}");
        }
        // Never replace a committed measurement silently.
        println!(
            "    -> {}{}",
            path.display(),
            if replacing { "  (replaced)" } else { "" }
        );
    }
    Ok(())
}

fn select(root: &Path, args: &RunArgs) -> Result<Vec<scenario::Scenario>, String> {
    let all = scenario::load_all(root)?;
    match (&args.scenario, args.all) {
        (Some(name), _) => all
            .into_iter()
            .find(|s| &s.name == name)
            .map(|s| vec![s])
            .ok_or_else(|| format!("no scenario named '{name}' (try `rbpmn-bench list`)")),
        (None, true) => Ok(all),
        (None, false) => Err(
            "name a scenario, or pass --all for the whole suite (try `rbpmn-bench list`)"
                .to_string(),
        ),
    }
}

#[allow(clippy::too_many_arguments)]
async fn population_run(
    root: &Path,
    database_url: &str,
    name: &str,
    sizes: Option<Vec<u32>>,
    samples: Option<u32>,
    reuse: bool,
    provisioned_by: &str,
) -> Result<(), String> {
    let scenario = scenario::load_all(root)?
        .into_iter()
        .find(|s| s.name == name)
        .ok_or_else(|| format!("no scenario named '{name}' (try `rbpmn-bench list`)"))?;
    println!("=== {} — {}", scenario.name, scenario.summary);
    let report = population::run(
        &scenario,
        &population::PopulationOptions {
            root: root.to_path_buf(),
            database_url: database_url.to_string(),
            provisioned_by: provisioned_by.to_string(),
            sizes,
            samples,
            reuse,
        },
    )
    .await?;
    print!("{}", report.render());
    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    let path = report.write(root)?;
    println!("-> {}", path.display());
    Ok(())
}

async fn micro_persisted(
    root: &Path,
    database_url: &str,
    iterations: u32,
    provisioned_by: &str,
) -> Result<(), String> {
    let report = micro::run(root, database_url, provisioned_by, iterations).await?;
    print!("{}", report.render());
    let path = report.write(root)?;
    println!("\n-> {}", path.display());
    Ok(())
}

/// Exits non-zero on a regression — the one place in this track that fails a
/// build. A missing baseline is not a failure: it is a machine that has not
/// recorded one yet, and refusing to build on it would be a gate that fails
/// for a reason unrelated to performance.
fn gate_micro(root: &Path, criterion_dir: &Path, threshold: f64) -> ExitCode {
    let current = match gate::measurements(criterion_dir) {
        Ok(current) => current,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let hardware = env::Hardware::detect(root);
    let path = gate::baseline_path(root, &hardware.host_id);
    let Ok(text) = std::fs::read_to_string(&path) else {
        println!(
            "no baseline for host {} — nothing to compare against.\n\
             Baselines are per machine and deliberately never committed (they live in \
             the gitignored benchmarks/.baselines/), because a baseline from another \
             machine is not a stricter check, it is a false one.\n\
             Record this machine's with: just bench-baseline\n\n\
             Measured now:",
            hardware.host_id
        );
        for (id, stat) in &current {
            println!(
                "  {id:<40} {:>10.1}ns  (noise ±{:.1}ns)",
                stat.median_ns, stat.noise_ns
            );
        }
        return ExitCode::SUCCESS;
    };
    let baseline: gate::Baseline = match serde_json::from_str(&text) {
        Ok(baseline) => baseline,
        Err(e) => {
            eprintln!("error: {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let verdict = gate::compare(&baseline, &current, threshold);
    print!("{}", gate::render(&verdict, threshold));
    println!(
        "baseline recorded {} on {} ({})",
        baseline.recorded_at,
        baseline.cpu_model,
        baseline.git_sha.as_deref().unwrap_or("no git sha")
    );
    if verdict.regressions > 0 {
        eprintln!(
            "\nerror: {} pure-core benchmarks are slower than {:.0}% plus this machine's \
             own measured noise for them. The suite touches no database, no IO and no \
             clock, so the likely cause is the semantic core — but check the sensitivity \
             column before believing a marginal one. If the change is intentional, \
             re-record the baseline with `just bench-baseline`.",
            verdict.regressions,
            threshold * 100.0
        );
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn record_baseline(root: &Path, criterion_dir: &Path) -> Result<(), String> {
    let benchmarks = gate::measurements(criterion_dir)?;
    let hardware = env::Hardware::detect(root);
    let checkout = env::Checkout::detect(root);
    if checkout.dirty {
        println!(
            "warning: recording a baseline from a dirty checkout — it will not be \
             reproducible from its git sha"
        );
    }
    let baseline = gate::Baseline {
        schema: gate::BASELINE_SCHEMA.to_string(),
        host_id: hardware.host_id.clone(),
        recorded_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        git_sha: checkout.git_sha.clone(),
        cpu_model: hardware.detected.cpu_model.clone(),
        benchmarks,
    };
    let path = gate::baseline_path(root, &hardware.host_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(&baseline).map_err(|e| e.to_string())?;
    std::fs::write(&path, format!("{json}\n")).map_err(|e| format!("{}: {e}", path.display()))?;
    println!(
        "recorded {} benchmarks for host {} -> {}",
        baseline.benchmarks.len(),
        baseline.host_id,
        path.display()
    );
    Ok(())
}

fn render_report(root: &Path, out: Option<&Path>) -> Result<(), String> {
    let markdown = report::render(root)?;
    match out {
        Some(path) => {
            std::fs::write(path, &markdown).map_err(|e| format!("{}: {e}", path.display()))?;
            println!("-> {}", path.display());
        }
        None => print!("{markdown}"),
    }
    Ok(())
}
