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

    match rbpmn_server::serve(config, engine).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::from(1)
        }
    }
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
