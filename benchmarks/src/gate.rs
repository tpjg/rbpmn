//! The micro-benchmark regression gate.
//!
//! This is the one place in the benchmark track that is allowed to *fail* a
//! build, and it is fenced accordingly:
//!
//! - It only ever compares the **pure core** suite (`benches/core_constructs.rs`),
//!   which touches no database, no network and no disk. That is the only
//!   layer deterministic enough for a threshold to mean something.
//! - It compares against a baseline recorded **on this machine**, which is
//!   why the baseline is **never committed**: it lives in the gitignored
//!   `benchmarks/.baselines/`, one file per host id. Absolute numbers do not
//!   travel between machines, so a baseline from another host is not a
//!   stricter check — it is a false one, and a committed one is an
//!   invitation to make exactly that mistake. Everyone records their own;
//!   without a matching baseline the gate reports, passes, and says how.
//! - **The baseline records each benchmark's noise, and the gate adds it to
//!   the threshold.** This is not belt-and-braces; it is the difference
//!   between a gate and a coin toss. The first version compared point
//!   estimates against a flat 25%, and firing it twice against *identical
//!   code* produced two "regressions" and a spread of −29% to +68%: on an
//!   Apple Silicon laptop a 1 µs benchmark bounces between performance and
//!   efficiency cores and criterion reports a median absolute deviation
//!   roughly equal to the median. A machine that measures noisily can only
//!   detect large regressions, and the honest thing is to say so — which
//!   [`render`] does, per benchmark, in the sensitivity line.
//!
//! What it is for: the accidental O(n²) in gateway handling, the allocation
//! added to the hot path of `step`, the clone that crept into the advancer.
//! Those show up as multiples, not as percents.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const BASELINE_SCHEMA: &str = "rbpmn-bench-micro/1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Baseline {
    pub schema: String,
    pub host_id: String,
    pub recorded_at: String,
    pub git_sha: Option<String>,
    pub cpu_model: String,
    /// By criterion benchmark id.
    pub benchmarks: BTreeMap<String, BenchStat>,
}

/// What the baseline remembers about one benchmark: where it sat, and how
/// hard it was to measure.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct BenchStat {
    /// Median nanoseconds per iteration. Median rather than mean: a
    /// micro-benchmark's mean is where an OS scheduling hiccup lands.
    pub median_ns: f64,
    /// This machine's measurement noise for this benchmark — the wider of
    /// criterion's median absolute deviation and half its confidence
    /// interval. Added to the threshold, so a quiet machine gates tightly
    /// and a noisy one gates loosely, both automatically.
    pub noise_ns: f64,
}

/// `benchmarks/.baselines/micro-baseline-<host-id>.json`.
///
/// A whole gitignored directory rather than a pattern inside the committed
/// `results/`: a directory that is entirely machine-local is much harder to
/// commit by accident than one file among several that are meant to be
/// committed.
pub fn baseline_path(root: &Path, host_id: &str) -> PathBuf {
    root.join(".baselines")
        .join(format!("micro-baseline-{host_id}.json"))
}

/// Median and noise, per benchmark, from criterion's own output.
pub fn measurements(criterion_dir: &Path) -> Result<BTreeMap<String, BenchStat>, String> {
    let mut out = BTreeMap::new();
    collect(criterion_dir, criterion_dir, &mut out)?;
    if out.is_empty() {
        return Err(format!(
            "no criterion estimates under {} — run the micro suite first \
             (`cargo bench -p rbpmn-bench --bench core_constructs`)",
            criterion_dir.display()
        ));
    }
    Ok(out)
}

fn collect(root: &Path, dir: &Path, out: &mut BTreeMap<String, BenchStat>) -> Result<(), String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    for entry in entries {
        let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
        if path.is_dir() {
            // `report/` is criterion's HTML; `base/` and `change/` are its
            // own baseline bookkeeping, which this gate deliberately does
            // not use — the committed per-host file is the baseline.
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name == "report" || name == "base" || name == "change" {
                continue;
            }
            collect(root, &path, out)?;
            continue;
        }
        if path.file_name().is_some_and(|n| n == "estimates.json")
            && path
                .parent()
                .and_then(|p| p.file_name())
                .is_some_and(|n| n == "new")
        {
            let id = path
                .parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.strip_prefix(root).ok())
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            let text =
                std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let value: serde_json::Value =
                serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
            let median = value
                .get("median")
                .and_then(|m| m.get("point_estimate"))
                .and_then(|p| p.as_f64());
            if let Some(median_ns) = median {
                let mad = value
                    .get("median_abs_dev")
                    .and_then(|m| m.get("point_estimate"))
                    .and_then(|p| p.as_f64())
                    .unwrap_or(0.0);
                let ci_half = value
                    .get("median")
                    .and_then(|m| m.get("confidence_interval"))
                    .and_then(|ci| {
                        Some(
                            (ci.get("upper_bound")?.as_f64()? - ci.get("lower_bound")?.as_f64()?)
                                / 2.0,
                        )
                    })
                    .unwrap_or(0.0);
                out.insert(
                    id,
                    BenchStat {
                        median_ns,
                        noise_ns: mad.max(ci_half),
                    },
                );
            }
        }
    }
    Ok(())
}

pub struct Verdict {
    pub rows: Vec<Row>,
    /// Benchmarks in the baseline that this run did not produce. Not a
    /// failure — a renamed benchmark is a normal edit — but it is printed,
    /// because a silently vanished benchmark is a gate that stopped
    /// checking something.
    pub missing: Vec<String>,
    pub regressions: usize,
}

pub struct Row {
    pub id: String,
    pub baseline_ns: f64,
    pub current_ns: f64,
    pub change: f64,
    /// What this benchmark would have to reach to count as a regression on
    /// this machine.
    pub limit_ns: f64,
    pub regressed: bool,
}

impl Row {
    /// The smallest slowdown this machine can actually detect for this
    /// benchmark, as a fraction. Printed, because a gate that cannot see a
    /// 40% regression should say so rather than imply it is watching.
    pub fn sensitivity(&self) -> f64 {
        (self.limit_ns - self.baseline_ns) / self.baseline_ns
    }
}

/// A benchmark is regressed when it is slower than the threshold *and*
/// clearly outside the noise the baseline recorded for it. Both conditions:
/// the threshold keeps a quiet machine from gating on 3% drift, the noise
/// term keeps a noisy one from gating on nothing at all.
pub fn compare(
    baseline: &Baseline,
    current: &BTreeMap<String, BenchStat>,
    threshold: f64,
) -> Verdict {
    let mut rows = Vec::new();
    let mut missing = Vec::new();
    let mut regressions = 0;
    for (id, base) in &baseline.benchmarks {
        let Some(stat) = current.get(id).copied() else {
            missing.push(id.clone());
            continue;
        };
        let limit_ns = base.median_ns * (1.0 + threshold) + base.noise_ns;
        let regressed = stat.median_ns > limit_ns;
        if regressed {
            regressions += 1;
        }
        rows.push(Row {
            id: id.clone(),
            baseline_ns: base.median_ns,
            current_ns: stat.median_ns,
            change: (stat.median_ns - base.median_ns) / base.median_ns,
            limit_ns,
            regressed,
        });
    }
    rows.sort_by(|a, b| b.change.partial_cmp(&a.change).expect("finite changes"));
    Verdict {
        rows,
        missing,
        regressions,
    }
}

pub fn render(verdict: &Verdict, threshold: f64) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "{:<40} {:>11} {:>11} {:>9} {:>13}\n",
        "benchmark", "baseline", "current", "change", "detects"
    ));
    for row in &verdict.rows {
        out.push_str(&format!(
            "{:<40} {:>9.1}ns {:>9.1}ns {:>+8.1}% {:>+12.0}%{}\n",
            elide(&row.id, 40),
            row.baseline_ns,
            row.current_ns,
            row.change * 100.0,
            row.sensitivity() * 100.0,
            if row.regressed { "  REGRESSION" } else { "" }
        ));
    }
    for id in &verdict.missing {
        out.push_str(&format!("{:<40} {:>11} (not run)\n", elide(id, 40), "—"));
    }
    let worst = verdict
        .rows
        .iter()
        .map(|r| r.sensitivity())
        .fold(0.0f64, f64::max);
    out.push_str(&format!(
        "\n{} benchmarks compared, {} regressed.\n\
         Threshold {:.0}% plus each benchmark's recorded measurement noise. On this \n\
         machine the least sensitive benchmark only detects a {:.0}% slowdown — that is \n\
         what its noise allows, not a choice.\n",
        verdict.rows.len(),
        verdict.regressions,
        threshold * 100.0,
        worst * 100.0,
    ));
    out
}

fn elide(s: &str, width: usize) -> String {
    if s.len() <= width {
        s.to_string()
    } else {
        format!("…{}", &s[s.len() - width + 1..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline(entries: &[(&str, f64, f64)]) -> Baseline {
        Baseline {
            schema: BASELINE_SCHEMA.into(),
            host_id: "test".into(),
            recorded_at: "2026-01-01T00:00:00Z".into(),
            git_sha: None,
            cpu_model: "test".into(),
            benchmarks: entries
                .iter()
                .map(|(k, median_ns, noise_ns)| {
                    (
                        k.to_string(),
                        BenchStat {
                            median_ns: *median_ns,
                            noise_ns: *noise_ns,
                        },
                    )
                })
                .collect(),
        }
    }

    fn current(entries: &[(&str, f64)]) -> BTreeMap<String, BenchStat> {
        entries
            .iter()
            .map(|(k, median_ns)| {
                (
                    k.to_string(),
                    BenchStat {
                        median_ns: *median_ns,
                        noise_ns: 0.0,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn a_slowdown_past_threshold_and_noise_is_a_regression() {
        let base = baseline(&[("step/flow", 100.0, 5.0)]);
        let verdict = compare(&base, &current(&[("step/flow", 200.0)]), 0.25);
        assert_eq!(verdict.regressions, 1);
        assert!(verdict.rows[0].regressed);
    }

    #[test]
    fn noise_inside_the_threshold_is_not() {
        let base = baseline(&[("step/flow", 100.0, 5.0)]);
        assert_eq!(
            compare(&base, &current(&[("step/flow", 115.0)]), 0.25).regressions,
            0
        );
    }

    /// The bug this design exists to prevent: on a machine whose measurement
    /// noise is as large as the measurement, a flat threshold fires on
    /// identical code. Observed for real — two "regressions" on an unchanged
    /// tree — before the noise term was added.
    #[test]
    fn a_noisy_machine_does_not_gate_on_its_own_jitter() {
        let base = baseline(&[("step/flow", 1000.0, 950.0)]);
        // +68%, which is what an unchanged benchmark actually did between
        // two runs on the machine this was developed on.
        let verdict = compare(&base, &current(&[("step/flow", 1680.0)]), 0.25);
        assert_eq!(verdict.regressions, 0);
        // ...and it says out loud that it can only see a 120% slowdown here.
        assert!(verdict.rows[0].sensitivity() > 1.0);
    }

    /// The other half: a noisy machine must still catch a real regression,
    /// or the noise term has simply switched the gate off.
    #[test]
    fn a_noisy_machine_still_catches_a_doubling() {
        let base = baseline(&[("step/flow", 1000.0, 950.0)]);
        assert_eq!(
            compare(&base, &current(&[("step/flow", 3000.0)]), 0.25).regressions,
            1
        );
    }

    #[test]
    fn a_speedup_is_never_a_regression() {
        let base = baseline(&[("step/flow", 100.0, 5.0)]);
        let verdict = compare(&base, &current(&[("step/flow", 40.0)]), 0.25);
        assert_eq!(verdict.regressions, 0);
        assert!(verdict.rows[0].change < 0.0);
    }

    #[test]
    fn a_benchmark_that_vanished_is_reported_not_ignored() {
        let base = baseline(&[("step/flow", 100.0, 5.0), ("step/gone", 100.0, 5.0)]);
        let verdict = compare(&base, &current(&[("step/flow", 100.0)]), 0.25);
        assert_eq!(verdict.missing, vec!["step/gone".to_string()]);
        assert_eq!(verdict.regressions, 0);
    }
}
