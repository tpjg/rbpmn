//! The result file: everything needed to reproduce the run, plus what it
//! measured.
//!
//! Written to `results/<scenario>-<mode>-<date>-<host-id>.json`, which is
//! **gitignored**: every file is stamped with the machine that produced it,
//! and committed it would stop being "what that laptop measured" and become
//! "rbpmn's numbers". The provenance block is what makes a file worth
//! keeping outside the repository — a number whose scenario hash, model
//! hash, seed, Postgres settings and hardware are not attached cannot be
//! compared with anything, including itself six months later.

use crate::env::{Checkout, Hardware};
use crate::pg::{PostgresFacts, Sample};
use crate::scenario::Scenario;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Bump when a field changes meaning. Readers (the report renderer, and
/// anyone comparing two results) check it.
pub const SCHEMA: &str = "rbpmn-bench/1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub schema: String,
    pub scenario: String,
    /// `saturation` (generate → drain) or `steady` (open-loop arrivals).
    pub mode: String,
    pub history: String,
    pub run_id: String,
    /// RFC 3339, UTC, from the harness's clock. Latencies never come from
    /// here — those are database time.
    pub started_at: String,
    pub checkout: Checkout,
    pub hardware: Hardware,
    pub postgres: PostgresFacts,
    pub provenance: Provenance,
    /// The scenario's own prose about its scope, copied in so a result file
    /// that outlives its TOML still says what it measured — and, more
    /// importantly, what it did not.
    pub scope: ScopeNotes,
    pub harness: HarnessConfig,
    pub measurements: Measurements,
    /// Empty when the monitor did not run.
    #[serde(default)]
    pub monitor: Vec<Sample>,
    /// Anything the run wants the reader to know before quoting it.
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provenance {
    pub scenario_sha256: String,
    pub models: BTreeMap<String, String>,
    pub tuning_sha256: Option<String>,
    pub seed: u64,
    /// The manifest actually deployed, as JSON — the other half of a
    /// definition, and the half no other engine's benchmark can show you.
    pub bindings: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessConfig {
    pub service_workers: u32,
    pub user_workers: u32,
    pub correlators: u32,
    pub schedulers: u32,
    pub db_pool: u32,
    pub warmup_instances: u32,
    pub measured_instances: u32,
    pub monitor_interval_secs: Option<f64>,
    /// Every rbpmn table was emptied before the run. The default, so that
    /// two runs of one scenario are comparable rather than depending on how
    /// much earlier work happened to be lying around.
    pub fresh_database: bool,
    /// `ANALYZE` ran on the instance and work-item tables after the backlog
    /// was parked and before the workers started. Without it the claim
    /// path's plan depends on when autovacuum last ran — measured at ~8x on
    /// one scenario. See `run::analyze_before_execute`.
    pub analyze_before_execute: bool,
    pub arrival_rate: Option<f64>,
    pub steady_duration_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Measurements {
    pub measured_instances: u32,
    pub completed_instances: i64,
    pub failed_instances: i64,
    /// Wall-clock seconds of the measured window (harness monotonic clock).
    pub duration_secs: f64,
    /// The headline: completed instances per second.
    pub throughput_instances_per_sec: f64,
    /// Where instance latency is measured *from*. `arrival` is start →
    /// terminal, the number that means what people assume it means.
    /// `drain` is drain-start → terminal, which is queue-inclusive and
    /// deliberately not comparable to `arrival`.
    pub latency_kind: String,
    pub latency_ms: Latency,
    /// Per-instance latencies in milliseconds, sorted. Capped — see
    /// `latencies_truncated`.
    pub latencies_ms: Vec<f64>,
    pub latencies_truncated: bool,
    pub work_items_completed: u64,
    pub inbox_queries: u64,
    pub messages_correlated: u64,
    pub correlate_retries: u64,
    pub events_written: i64,
    pub events_per_instance: f64,
    pub event_bytes_per_instance: f64,
    pub backpressure: Backpressure,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Latency {
    pub count: usize,
    pub min: f64,
    pub mean: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
}

impl Latency {
    /// `sorted` must be ascending; the caller sorts once and keeps the array.
    pub fn from_sorted(sorted: &[f64]) -> Latency {
        if sorted.is_empty() {
            return Latency::default();
        }
        let at = |q: f64| {
            // Nearest-rank, which is what a p99 over a few thousand samples
            // should be: no interpolation between two observations that both
            // actually happened.
            let rank = (q * sorted.len() as f64).ceil().max(1.0) as usize;
            sorted[rank.min(sorted.len()) - 1]
        };
        Latency {
            count: sorted.len(),
            min: sorted[0],
            mean: sorted.iter().sum::<f64>() / sorted.len() as f64,
            p50: at(0.50),
            p95: at(0.95),
            p99: at(0.99),
            max: sorted[sorted.len() - 1],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Backpressure {
    /// False is a claim, not a default: in steady mode it means every
    /// arrival went out on time; in saturation mode there is no arrival
    /// process to fall behind, and `reason` says so.
    pub occurred: bool,
    pub reason: String,
    /// Steady mode: how far behind schedule the latest arrival was.
    #[serde(default)]
    pub max_arrival_lag_ms: Option<f64>,
    #[serde(default)]
    pub arrivals_late: Option<u64>,
}

/// Latencies embedded verbatim before the array is truncated. Two thousand
/// f64s is ~20 kB of JSON; a hundred thousand is not a file anyone will
/// diff.
pub const MAX_RAW_LATENCIES: usize = 20_000;

impl RunResult {
    /// `results/<scenario>-<mode>-<date>-<host-id>.json`.
    ///
    /// The mode is in the name deliberately, and it is the one deviation
    /// from the layout this track was specified with. Without it, running a
    /// scenario in `steady` mode overwrites the same day's `saturation`
    /// result on the same machine — two different measurements, one
    /// filename, the second silently replacing the first. Re-running the
    /// *same* mode still replaces, which is right, and the caller says so
    /// out loud — which matters more than it used to, because `results/` is
    /// gitignored and nothing recovers an overwritten measurement.
    pub fn path(&self, root: &Path) -> PathBuf {
        let date = self.started_at.get(..10).unwrap_or("unknown-date");
        root.join("results").join(format!(
            "{}-{}-{date}-{}.json",
            self.scenario, self.mode, self.hardware.host_id
        ))
    }

    /// True when [`RunResult::write`] would replace an existing file.
    pub fn replaces(&self, root: &Path) -> bool {
        self.path(root).exists()
    }

    pub fn write(&self, root: &Path) -> Result<PathBuf, String> {
        let path = self.path(root);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, format!("{json}\n"))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(path)
    }
}

pub fn scenario_hash(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeNotes {
    pub summary: String,
    pub measures: Vec<String>,
    pub excludes: Vec<String>,
    /// Model shape, so a reader can see what an instance actually costs
    /// without opening the .bpmn.
    pub elements: usize,
    pub service_tasks: usize,
    pub user_tasks: usize,
    pub timers: usize,
    pub message_catches: usize,
    pub subprocesses: usize,
}

impl ScopeNotes {
    pub fn new(scenario: &Scenario, model: &crate::model::Model) -> ScopeNotes {
        ScopeNotes {
            summary: scenario.summary.clone(),
            measures: scenario.measures.clone(),
            excludes: scenario.excludes.clone(),
            elements: model.elements,
            service_tasks: model.service_tasks.len(),
            user_tasks: model.user_tasks.len(),
            timers: model.timers,
            message_catches: model.message_catches,
            subprocesses: model.subprocesses,
        }
    }
}
