//! The monitor: a separate process that samples the database while a run is
//! in flight.
//!
//! Separate on purpose. A sampler sharing a runtime with the load reports
//! the runtime's scheduling delays as the database's latency, and a sampler
//! sharing the harness's connection pool takes a connection away from the
//! thing it is measuring. This one runs standalone (`rbpmn-bench monitor`),
//! opens two connections of its own, appends one JSON object per sample, and
//! stops when it is killed — so nothing about it needs the run to end
//! cleanly.

use crate::pg;
use sqlx::postgres::PgPoolOptions;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

pub async fn run(database_url: &str, interval_secs: f64, out: Option<&Path>) -> Result<(), String> {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(10))
        .connect(database_url)
        .await
        .map_err(|e| {
            format!(
                "monitor connecting to {}: {e}",
                crate::run::redact(database_url)
            )
        })?;

    let mut file = match out {
        Some(path) => {
            Some(std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?)
        }
        None => None,
    };

    let started = Instant::now();
    // The first window opens at the monitor's own start, so the first
    // sample's percentiles cover only what finished after it was watching.
    let mut since: chrono::DateTime<chrono::Utc> = sqlx::query_scalar("select clock_timestamp()")
        .fetch_one(&pool)
        .await
        .map_err(|e| format!("monitor reading database time: {e}"))?;

    let interval = Duration::from_secs_f64(interval_secs.max(0.05));
    loop {
        let tick = Instant::now();
        match pg::sample(&pool, started.elapsed().as_secs_f64(), since).await {
            Ok((sample, now)) => {
                since = now;
                let line = serde_json::to_string(&sample).map_err(|e| e.to_string())?;
                match file.as_mut() {
                    Some(file) => {
                        writeln!(file, "{line}").map_err(|e| format!("writing a sample: {e}"))?;
                        file.flush()
                            .map_err(|e| format!("flushing a sample: {e}"))?;
                    }
                    None => println!("{line}"),
                }
            }
            Err(e) => {
                // The database going away mid-run is the run's problem, not
                // the monitor's: say so on stderr and keep sampling, so the
                // samples either side of the outage are both in the file.
                eprintln!("monitor: sample failed: {e}");
            }
        }
        let elapsed = tick.elapsed();
        if elapsed < interval {
            tokio::time::sleep(interval - elapsed).await;
        }
    }
}
