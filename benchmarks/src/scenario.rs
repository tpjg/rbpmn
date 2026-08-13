//! Scenario definitions: one TOML per benchmark, mirroring the fixture
//! corpus's convention that the *data* is the specification and the runner is
//! generic. A scenario says what to deploy, how to seed the variable
//! document, who does the work, and — in prose that ends up in the report —
//! what the numbers do and do not mean.
//!
//! Unknown keys are rejected (`deny_unknown_fields`). A typo in a benchmark
//! definition that silently falls back to a default is a measurement that
//! quietly describes a different system than the one you meant to measure.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Which event kinds the engine writes. The axis exists because history
/// write volume is the single biggest performance lever in this design and
/// the benchmark is where that gets proven — but per-definition event-kind
/// filtering is a *roadmap* item, deliberately not shipped (it changes the
/// event stream's completeness contract; see the design brief, phase 7).
///
/// So the axis is wired and only one value runs. The other two are refused
/// loudly, naming the feature they need, rather than silently measuring
/// something else and labelling it `off`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum History {
    /// Every event written — what the engine does today, the only
    /// configuration that exists.
    Full,
    /// Instance-level events only. Needs per-definition event-kind filtering.
    Instance,
    /// No events at all. Needs the same.
    Off,
}

impl History {
    pub fn as_str(self) -> &'static str {
        match self {
            History::Full => "full",
            History::Instance => "instance",
            History::Off => "off",
        }
    }

    /// The loud refusal. Returns the reason a configuration cannot run, or
    /// `None` when it can.
    pub fn unsupported_reason(self) -> Option<String> {
        match self {
            History::Full => None,
            other => Some(format!(
                "history level '{}' is not implemented: per-definition event-kind \
                 filtering is a roadmap item (bpmn-engine-design.md, phase 7 — it \
                 changes the event stream's completeness contract, so it is a \
                 separate feature with its own design round). Only 'full' can be \
                 measured today. What the benchmark *can* say about the lever is \
                 recorded in every result file as events_written / \
                 event_bytes_per_instance — see benchmarks/README.md, \
                 'The history axis'.",
                other.as_str()
            )),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// Must equal the file stem — the result filename is built from it.
    pub name: String,
    /// File name inside `benchmarks/models/`.
    pub model: String,
    pub summary: String,
    /// What the measured path includes. Rendered into the report verbatim.
    pub measures: Vec<String>,
    /// What it deliberately does not include. Also rendered verbatim: a
    /// headline number without its conditions is the thing this whole track
    /// exists to avoid publishing.
    pub excludes: Vec<String>,
    pub bindings: BindingsSpec,
    pub workload: Workload,
    pub execute: Execute,
    #[serde(default)]
    pub steady: Option<Steady>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BindingsSpec {
    /// Topic every service task resolves to, unless overridden below. One
    /// topic per scenario keeps the worker pool sizing honest; the
    /// many-tasks-to-one-topic relationship is the design's normal case.
    pub service_topic: String,
    /// Topic every user task resolves to. Absent when the model has none.
    #[serde(default)]
    pub user_topic: Option<String>,
    /// Per-element overrides, for models that want two pools.
    #[serde(default)]
    pub topics: BTreeMap<String, String>,
    /// Message correlation, bound in the manifest and never in the XML.
    #[serde(default)]
    pub correlation: Option<CorrelationSpec>,
    /// Filterable fields (`declare_index`) — performance only.
    #[serde(default)]
    pub indexes: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CorrelationSpec {
    /// The catching element id.
    pub element: String,
    /// The message *name* as it appears in the XML.
    pub message: String,
    /// FEEL qualified name into the variable document.
    pub key: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Workload {
    /// Instances run and discarded before measurement starts. They stay in
    /// the database — a warm cache is the point, and so is the table size
    /// they leave behind.
    pub warmup: u32,
    /// Instances in the measured batch.
    pub instances: u32,
    /// Seeds the variable documents. Recorded in the result file; the same
    /// seed reproduces the same documents on any machine.
    pub seed: u64,
    #[serde(default = "default_history")]
    pub history: History,
    /// The seeded variable document, one entry per FEEL qualified name.
    #[serde(default)]
    pub fields: Vec<FieldSpec>,
}

fn default_history() -> History {
    History::Full
}

/// One field of the seeded variable document, addressed by its FEEL
/// qualified name (`order.id`) — the same syntax conditions and correlation
/// keys use, so what the model reads and what the harness writes are spelled
/// the same way.
/// No `deny_unknown_fields` here, and it is not an oversight: serde does not
/// support it alongside `flatten`. The strictness lives one level down, on
/// [`FieldValue`], which is where a mistyped generator key would be.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FieldSpec {
    pub path: String,
    #[serde(flatten)]
    pub value: FieldValue,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum FieldValue {
    /// Uniform choice from `values`, drawn from the seeded RNG.
    Pick {
        values: Vec<serde_json::Value>,
    },
    /// `true`/`false`, uniform.
    PickBool,
    /// `<prefix><run-id>-<index>` — unique per instance and *reproducible*
    /// from (run id, index) alone, which is what lets the correlator know
    /// the keys it must deliver without reading them back out of the
    /// database.
    Unique {
        prefix: String,
    },
    Constant {
        value: serde_json::Value,
    },
    /// A string of `bytes` filler characters. Variable-document size is a
    /// real cost (every step reads and rewrites the document); this is the
    /// knob that exposes it.
    Filler {
        bytes: usize,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Execute {
    /// Push-mode worker loops serving `service_topic`.
    pub service_workers: u32,
    /// Pull-mode loops serving `user_topic` (claim, query, complete).
    #[serde(default)]
    pub user_workers: u32,
    /// Concurrent `correlate` callers, for scenarios that wait on a message.
    #[serde(default)]
    pub correlators: u32,
    /// Database connections the harness opens. Recorded in the result and
    /// reported next to throughput, because a benchmark's connection count
    /// is half of what its numbers mean.
    pub db_pool: u32,
    /// The merge patch each service handler returns. Defaults to a small
    /// object; an empty patch would skip a write path a real handler always
    /// takes.
    #[serde(default)]
    pub patch: Option<serde_json::Value>,
    /// Inbox query performed inside the measured path for user tasks.
    #[serde(default)]
    pub user_query: Option<UserQuery>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UserQuery {
    /// Field the filter matches on — declare it in `bindings.indexes` or the
    /// query is a sequential scan (correct, just slower; both are worth
    /// measuring, which is why this is not wired to the index list).
    pub filter_field: String,
    /// Values the workers partition themselves across, so the whole inbox
    /// drains rather than one third of it.
    pub filter_values: Vec<String>,
    /// Call `count_tasks` before claiming — the dashboard indication a task
    /// list renders alongside its rows.
    #[serde(default)]
    pub count_first: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Steady {
    /// Open-loop arrivals per second.
    pub arrival_rate: f64,
    pub duration_secs: u64,
}

impl Scenario {
    pub fn load(path: &Path) -> Result<Scenario, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let scenario: Scenario =
            toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if scenario.name != stem {
            return Err(format!(
                "{}: scenario name '{}' must match the file stem '{stem}' — \
                 result files are named from it",
                path.display(),
                scenario.name
            ));
        }
        Ok(scenario)
    }

    pub fn model_path(&self, root: &Path) -> PathBuf {
        root.join("models").join(&self.model)
    }

    /// The manifest this scenario deploys with. Built from the model's own
    /// element list so that adding a task to a model cannot leave it
    /// silently unbound — an unbound service task would default its topic to
    /// its element id and fail `unresolved-topic` at deploy, which is the
    /// loud outcome, but naming the mapping explicitly is what the result
    /// file records.
    pub fn bindings(&self, model: &crate::model::Model) -> rbpmn_core::Bindings {
        let mut bindings = rbpmn_core::Bindings::new();
        for element in &model.service_tasks {
            let topic = self
                .bindings
                .topics
                .get(element)
                .cloned()
                .unwrap_or_else(|| self.bindings.service_topic.clone());
            bindings = bindings.topic(element, topic);
        }
        for element in &model.user_tasks {
            if let Some(topic) = self
                .bindings
                .topics
                .get(element)
                .cloned()
                .or_else(|| self.bindings.user_topic.clone())
            {
                bindings = bindings.topic(element, topic);
            }
        }
        if let Some(correlation) = &self.bindings.correlation {
            bindings = bindings.correlation(&correlation.element, &correlation.key);
        }
        for field in &self.bindings.indexes {
            bindings = bindings.index(field);
        }
        bindings
    }

    /// Topics the environment must cover before this scenario deploys.
    pub fn service_topics(&self, model: &crate::model::Model) -> Vec<String> {
        let mut topics: Vec<String> = model
            .service_tasks
            .iter()
            .map(|element| {
                self.bindings
                    .topics
                    .get(element)
                    .cloned()
                    .unwrap_or_else(|| self.bindings.service_topic.clone())
            })
            .collect();
        topics.sort();
        topics.dedup();
        topics
    }

    pub fn user_topic(&self) -> Option<&str> {
        self.bindings.user_topic.as_deref()
    }
}

/// Every scenario in `benchmarks/scenarios/`, in file order.
pub fn load_all(root: &Path) -> Result<Vec<Scenario>, String> {
    let dir = root.join("scenarios");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<_, _>>()
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    paths.retain(|p| p.extension().is_some_and(|ext| ext == "toml"));
    paths.sort();
    paths.iter().map(|p| Scenario::load(p)).collect()
}
