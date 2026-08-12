use rbpmn_server::Config;
use std::process::ExitCode;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let config = match Config::from_env() {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    let engine = match build_engine().await {
        Ok(engine) => engine,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };

    let workers: usize = std::env::var("RBPMN_WORKERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    for _ in 0..workers {
        let engine = engine.clone();
        tokio::spawn(async move {
            engine
                .run_worker(rbpmn_engine::WorkerOptions::default())
                .await
        });
    }

    // One timer scheduler per process; replicas compete safely (the timer
    // row's delete commits with the step — exactly-once by construction).
    {
        let engine = engine.clone();
        tokio::spawn(async move {
            engine
                .run_scheduler(rbpmn_engine::SchedulerOptions::default())
                .await
        });
    }

    // Retention is opt-in twice over: no sweeper runs unless the operator
    // names a policy, and the policy they name can still be "forever". A
    // server started without RBPMN_RETAIN_* deletes nothing, ever.
    match retention_options() {
        Ok(Some(options)) => {
            tracing::info!(
                runtime = ?options.default_policy.retain_runtime,
                history = ?options.default_policy.retain_history,
                "retention sweeper enabled"
            );
            let engine = engine.clone();
            tokio::spawn(async move { engine.run_retention(options).await });
        }
        Ok(None) => {}
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    }

    match rbpmn_server::serve(config, engine).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::from(1)
        }
    }
}

/// The sweeper's default policy, from the environment. `RBPMN_RETAIN_RUNTIME`
/// and `RBPMN_RETAIN_HISTORY` are durations in **days** (fractions allowed);
/// `forever` is spelled by omitting one. Neither set means no sweeper at all,
/// which is a different and even safer thing than a sweeper that keeps
/// everything: nothing is even scanned.
///
/// Per-definition overrides do not live here — they are registration-time
/// wiring in the embedding application, like topics and indexes.
fn retention_options() -> Result<Option<rbpmn_engine::RetentionOptions>, String> {
    fn days(name: &str) -> Result<Option<std::time::Duration>, String> {
        match std::env::var(name) {
            Err(_) => Ok(None),
            Ok(raw) => {
                let value: f64 = raw
                    .trim()
                    .parse()
                    .map_err(|_| format!("invalid {name} '{raw}' (expected days, e.g. 30)"))?;
                if !value.is_finite() || value < 0.0 {
                    return Err(format!(
                        "invalid {name} '{raw}' (expected a positive number)"
                    ));
                }
                Ok(Some(std::time::Duration::from_secs_f64(
                    value * 24.0 * 3600.0,
                )))
            }
        }
    }
    let runtime = days("RBPMN_RETAIN_RUNTIME")?;
    let history = days("RBPMN_RETAIN_HISTORY")?;
    if runtime.is_none() && history.is_none() {
        return Ok(None);
    }
    let policy = rbpmn_engine::RetentionPolicy::new(runtime, history).map_err(|e| e.to_string())?;
    Ok(Some(rbpmn_engine::RetentionOptions::new(policy)))
}

/// Environment before definitions: wire handlers and declared topics from
/// operator config, then re-validate persisted definitions against exactly
/// that environment — a drifted replica refuses to start.
async fn build_engine() -> Result<rbpmn_engine::Engine, String> {
    let url = std::env::var("RBPMN_DATABASE_URL")
        .map_err(|_| "RBPMN_DATABASE_URL is required (postgres://...)".to_string())?;
    let pool = rbpmn_engine::connect(&url)
        .await
        .map_err(|e| format!("cannot connect to Postgres: {e}"))?;

    let engine = rbpmn_engine::Engine::builder(pool).build();
    engine
        .migrate()
        .await
        .map_err(|e| format!("migrations failed: {e}"))?;

    if let Ok(topics) = std::env::var("RBPMN_TOPICS") {
        for topic in topics.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            engine
                .declare_topic(topic)
                .await
                .map_err(|e| format!("cannot persist topic declaration '{topic}': {e}"))?;
        }
    }
    // Converge config with previously persisted declarations (API-declared
    // topics survive restarts; replicas see each other's declarations).
    engine
        .sync_environment()
        .await
        .map_err(|e| format!("cannot load persisted environment: {e}"))?;
    // RBPMN_HTTP_HANDLERS="topic=https://internal/x;other=https://internal/y"
    if let Ok(handlers) = std::env::var("RBPMN_HTTP_HANDLERS") {
        for entry in handlers.split(';').map(str::trim).filter(|e| !e.is_empty()) {
            let Some((topic, url)) = entry.split_once('=') else {
                return Err(format!(
                    "bad RBPMN_HTTP_HANDLERS entry '{entry}' (expected topic=url)"
                ));
            };
            engine.register_handler(
                topic.trim(),
                Arc::new(rbpmn_engine::HttpPostHandler::new(url.trim())),
            );
        }
    }

    let drift = engine
        .check_active_definitions()
        .await
        .map_err(|e| format!("startup re-validation failed: {e}"))?;
    if !drift.is_empty() {
        let mut msg = String::from(
            "refusing to start: the environment no longer covers persisted definitions\n",
        );
        for d in &drift {
            msg.push_str(&format!("  {d}\n"));
        }
        return Err(msg);
    }
    Ok(engine)
}
