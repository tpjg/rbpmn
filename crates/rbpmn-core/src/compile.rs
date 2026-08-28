//! Definitions -> ExecutableProcess: the front door of the semantic core.
//!
//! Compilation re-runs the linter (loudly reject, even for library users who
//! bypass deploy) and additionally refuses everything the *current phase*
//! cannot execute — elements the linter accepts because they are part of the
//! v1 model surface, but whose runtime arrives in a later phase. A model that
//! compiles is guaranteed to run to quiescence without ever hitting an
//! unimplemented element.

use rbpmn_model::model::*;
use rbpmn_model::{Diagnostic, condition, has_errors};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type NodeIx = usize;
pub type FlowIx = usize;
/// Index into [`ExecutableProcess`]'s scope table. `0` is the process root;
/// every embedded subprocess adds one. Static structure — the *runtime*
/// scope instances a token lives in are `ScopeId`s in the instance state.
pub type ScopeIx = usize;

/// The per-definition wiring from the deployment manifest: element ->
/// work-item topic, message element -> correlation key (a FEEL qualified
/// name into the instance variables), business-rule task -> decision, and
/// task -> config. Unmapped tasks default to their element id; correlations
/// have **no default** — every message catch must be mapped or compilation
/// fails (`message-has-correlation`) — and neither does [`Bindings::config`],
/// which is why a config entry binding nothing is an error where a stale
/// topic is not (`config-binds-task`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Bindings {
    #[serde(default)]
    pub topics: BTreeMap<String, String>,
    #[serde(default)]
    pub correlations: BTreeMap<String, String>,
    /// Variables fields the application filters/counts tasks by — the
    /// engine creates a partial expression index per entry at deploy
    /// (`declare_index`). Entirely optional performance declarations.
    ///
    /// Two spellings, one meaning: a bare string is the definition-scoped
    /// default (`"channel"`), an object names a scope
    /// (`{"field": "order_no", "scope": "shared"}`). See [`IndexScope`] for
    /// what a shared declaration asserts — it is a promise the engine cannot
    /// check.
    #[serde(default)]
    pub indexes: std::collections::BTreeSet<IndexDeclaration>,
    /// Business-rule task -> the decision it invokes. This is the spot where
    /// every other engine writes `camunda:decisionRef` into the XML; here it
    /// is manifest data, versioned with the definition and reviewable in git
    /// next to it (`docs/dmn.md`, D5).
    #[serde(default)]
    pub decisions: BTreeMap<String, DecisionBinding>,
    /// Task -> the configuration it is invoked with: free JSON, delivered
    /// beside the variables on every work item the element produces, and
    /// **never interpreted** — not read, not resolved, not evaluated. One
    /// handler on one topic, configured differently at each call site, which
    /// is what every other engine writes into the XML as `zeebe:taskHeaders`
    /// or `flowable:field` (`docs/design/task-config.md`).
    ///
    /// It is *model content*, not runtime configuration: it is inside
    /// `content_hash` and pinned with the instance, so changing it is a
    /// deploy by construction. Anything that must differ per environment or
    /// change without one belongs to the environment half, or to the
    /// application's own store keyed by the `(definition_id,
    /// definition_version)` every claimed task carries.
    ///
    /// Skipped when empty, and that is load-bearing rather than tidy: the
    /// serialized manifest is hashed, so a group that always appeared would
    /// change every existing `content_hash` and allocate a new version of
    /// every definition on the first redeploy after the upgrade.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub config: BTreeMap<String, serde_json::Value>,
}

/// What a declared index covers — the difference between
/// [`Bindings::index`] and [`Bindings::shared_index`].
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum IndexScope {
    /// One index per (definition key, field), partial on `definition_key`.
    /// The default, and the shape a `TaskFilter`'s literal definition key
    /// lets the planner prove — which keeps the index as small as the one
    /// definition it serves.
    #[default]
    Definition,
    /// One index per field, shared by every definition that declares it.
    ///
    /// For the lookup that spans definitions: a business identifier the
    /// application hoists into `variables` with the same meaning everywhere
    /// — an order number, a customer reference, an external case id — that a
    /// user quotes and the application must resolve to whichever instance
    /// carries it, without knowing which workflow or which deployment that
    /// is. Postgres can prove a partial index's predicate only from an
    /// equality against a constant, so `definition_key = any($1)` cannot use
    /// the definition-scoped indexes at all: it plans as a bitmap scan on the
    /// definition-key index with the hoisted field demoted to a recheck
    /// filter.
    ///
    /// **What the engine cannot check.** A shared declaration asserts that
    /// the field name means the *same thing* in every definition that
    /// declares it. `variables` is opaque to rbpmn by design — nothing here
    /// verifies that, and nothing can. It is the application's contract, and
    /// declaring `shared` is the application asserting it.
    Shared,
}

/// One entry of [`Bindings::indexes`]: a variables field, and the scope of
/// the index that serves it.
///
/// Ordered by field first, so a manifest of definition-scoped entries keeps
/// the plain lexicographic order it has always had.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndexDeclaration {
    pub field: String,
    pub scope: IndexScope,
}

impl IndexScope {
    /// The manifest spelling — what diagnostics and log lines name.
    pub fn as_str(self) -> &'static str {
        match self {
            IndexScope::Definition => "definition",
            IndexScope::Shared => "shared",
        }
    }
}

impl IndexDeclaration {
    pub fn definition(field: impl Into<String>) -> Self {
        IndexDeclaration {
            field: field.into(),
            scope: IndexScope::Definition,
        }
    }

    pub fn shared(field: impl Into<String>) -> Self {
        IndexDeclaration {
            field: field.into(),
            scope: IndexScope::Shared,
        }
    }
}

/// A definition-scoped entry serializes as the bare string it has always
/// been — that is not cosmetic. `deploy` hashes the serialized manifest into
/// `content_hash`, so widening every existing entry to an object would
/// allocate a new definition version for wiring that did not change. It also
/// normalizes: `{"field": "f", "scope": "definition"}` and `"f"` are the same
/// wiring and hash the same.
impl Serialize for IndexDeclaration {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct as _;
        match self.scope {
            IndexScope::Definition => serializer.serialize_str(&self.field),
            IndexScope::Shared => {
                let mut entry = serializer.serialize_struct("IndexDeclaration", 2)?;
                entry.serialize_field("field", &self.field)?;
                entry.serialize_field("scope", &self.scope)?;
                entry.end()
            }
        }
    }
}

/// Both spellings in, strictly: a bare string is definition-scoped, an object
/// names its scope. An unknown scope or an unknown key is refused here rather
/// than defaulted — a manifest that says something rbpmn does not understand
/// must not deploy as something it does.
impl<'de> Deserialize<'de> for IndexDeclaration {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct DeclVisitor;

        impl<'de> serde::de::Visitor<'de> for DeclVisitor {
            type Value = IndexDeclaration;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str(
                    "a field name, or {\"field\": \"...\", \"scope\": \"definition\"|\"shared\"}",
                )
            }

            fn visit_str<E: serde::de::Error>(self, field: &str) -> Result<Self::Value, E> {
                Ok(IndexDeclaration::definition(field))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut field: Option<String> = None;
                let mut scope: Option<IndexScope> = None;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "field" => {
                            if field.is_some() {
                                return Err(serde::de::Error::duplicate_field("field"));
                            }
                            field = Some(map.next_value()?);
                        }
                        "scope" => {
                            if scope.is_some() {
                                return Err(serde::de::Error::duplicate_field("scope"));
                            }
                            // The derived enum's own error names the valid
                            // scopes, which is exactly the message wanted.
                            scope = Some(map.next_value()?);
                        }
                        other => {
                            return Err(serde::de::Error::unknown_field(
                                other,
                                &["field", "scope"],
                            ));
                        }
                    }
                }
                Ok(IndexDeclaration {
                    field: field.ok_or_else(|| serde::de::Error::missing_field("field"))?,
                    scope: scope.unwrap_or_default(),
                })
            }
        }

        deserializer.deserialize_any(DeclVisitor)
    }
}

/// Which decision a business-rule task invokes, and where its answer lands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionBinding {
    /// The invocable's name, as the DMN artifact declares it. Bound by name
    /// rather than by (namespace, model, name): a deployment's artifacts are
    /// its own, so the short name is what a modeler knows. Two artifacts
    /// defining the same name make *this* binding ambiguous, and deploy
    /// refuses it rather than picking one — `correlate`'s discipline, applied
    /// to decisions.
    pub decision: String,
    /// Where the answer goes in the variable document: a FEEL qualified name
    /// (`order.discount`), the same syntax correlation keys use. Required —
    /// a decision whose answer went nowhere would be a task that runs and
    /// changes nothing.
    pub result: String,
}

impl Bindings {
    /// Fluent construction for the Rust library path. The standalone server
    /// deserializes the same struct from the deploy body's `bindings` JSON —
    /// two syntaxes, one manifest, one validation path.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn topic(mut self, element_id: impl Into<String>, topic: impl Into<String>) -> Self {
        self.topics.insert(element_id.into(), topic.into());
        self
    }

    /// Bind a message element to its correlation key — a FEEL qualified name
    /// (`order.id`) evaluated against the instance variables when the
    /// subscription is armed. Registered here, never in the XML.
    pub fn correlation(
        mut self,
        element_id: impl Into<String>,
        feel_name: impl Into<String>,
    ) -> Self {
        self.correlations
            .insert(element_id.into(), feel_name.into());
        self
    }

    /// Declare a filterable variables field, scoped to this definition
    /// (optional, performance only).
    pub fn index(mut self, field: impl Into<String>) -> Self {
        self.indexes.insert(IndexDeclaration {
            field: field.into(),
            scope: IndexScope::Definition,
        });
        self
    }

    /// Declare a filterable variables field **shared across definitions** —
    /// one index serving every definition that declares the same field, for
    /// the lookup that does not know which workflow carries the value.
    ///
    /// This asserts a contract the engine cannot verify; read
    /// [`IndexScope::Shared`] before using it.
    pub fn shared_index(mut self, field: impl Into<String>) -> Self {
        self.indexes.insert(IndexDeclaration {
            field: field.into(),
            scope: IndexScope::Shared,
        });
        self
    }

    /// Bind a business-rule task to the decision it invokes and the variable
    /// path its answer lands on. Registered here, never in the XML.
    pub fn decision(
        mut self,
        element_id: impl Into<String>,
        decision: impl Into<String>,
        result: impl Into<String>,
    ) -> Self {
        self.decisions.insert(
            element_id.into(),
            DecisionBinding {
                decision: decision.into(),
                result: result.into(),
            },
        );
        self
    }

    /// Configure one task: free JSON, delivered on every work item that
    /// element produces. Must be a JSON **object** — a single value is
    /// spelled `{"template": "warning_first"}`, which leaves room to add a
    /// second key later without changing the shape.
    ///
    /// The object rule is checked by `config-binds-task` rather than by the
    /// type, so the fluent path and the JSON path fail the same way, with a
    /// diagnostic naming the element instead of a parse error naming a byte
    /// offset.
    pub fn config(
        mut self,
        element_id: impl Into<String>,
        config: impl Into<serde_json::Value>,
    ) -> Self {
        self.config.insert(element_id.into(), config.into());
        self
    }
}

/// When an armed timer becomes due — the raw ISO-8601 text from the model
/// (lint-validated). The core never interprets it: durations and dates are
/// resolved to a `due_at` by the projection using **database time**; in the
/// pure core, time only ever enters as a `FireTimer` command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimerDue {
    Duration(String),
    Date(String),
    /// A repeating cycle (`R[n]/P…`, `R[n]/<datetime>/P…`), only ever on a
    /// non-interrupting boundary. The core keeps the text and the fire count
    /// (`TimerState::remaining`); every instant — the first due, and each
    /// re-arm as *previous due + period* — is the projection's, computed
    /// from `rbpmn_model::iso8601::split_cycle`.
    Cycle(String),
}

/// Which ISO-8601 shape a variable-sourced timer must resolve to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimerKind {
    Duration,
    Date,
    Cycle,
}

impl TimerKind {
    /// Is `text` a valid literal of this kind? The compiled-side counterpart
    /// of `rbpmn_model::TimerSpec::literal_check`, over the same validators.
    pub fn validate(self, text: &str) -> Result<(), String> {
        match self {
            TimerKind::Duration => rbpmn_model::iso8601::validate_duration(text),
            TimerKind::Date => rbpmn_model::iso8601::validate_datetime(text),
            TimerKind::Cycle => rbpmn_model::iso8601::validate_cycle(text),
        }
    }

    /// The resolved form of a validated `text`.
    pub fn due(self, text: &str) -> TimerDue {
        match self {
            TimerKind::Duration => TimerDue::Duration(text.to_string()),
            TimerKind::Date => TimerDue::Date(text.to_string()),
            TimerKind::Cycle => TimerDue::Cycle(text.to_string()),
        }
    }
}

/// A compiled timer spec: a literal validated at deploy, or a FEEL qualified
/// name read from the variable document when the timer is armed.
///
/// The two are deliberately different *types* rather than one enum with a
/// "maybe resolved" flag: [`TimerDue`] is what the core emits and the
/// projection stores, so making the resolved form unrepresentable-until-
/// resolved means no arming path can accidentally hand an unresolved
/// expression to the SQL cast. That cast is where an invalid value would
/// abort the whole step transaction — leaving the token at its *previous*
/// wait state and a worker retrying into the same failure forever.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TimerSource {
    Literal(TimerDue),
    Variable { kind: TimerKind, path: Vec<String> },
}

impl TimerSource {
    /// Resolve against the variable document. `Err` carries a message fit for
    /// an operator reading an incident: what was looked up, what was found,
    /// and why it cannot be a deadline.
    pub fn resolve(&self, variables: &serde_json::Value) -> Result<TimerDue, String> {
        let (kind, path) = match self {
            TimerSource::Literal(due) => return Ok(due.clone()),
            TimerSource::Variable { kind, path } => (kind, path),
        };
        let name = path.join(".");
        let value = rbpmn_model::condition::resolve_path(variables, path);
        let serde_json::Value::String(text) = value else {
            return Err(format!(
                "'{name}' is {}, and a timer needs an ISO-8601 string",
                describe(value)
            ));
        };
        match kind.validate(text) {
            Ok(()) => Ok(kind.due(text)),
            Err(why) => Err(format!("'{name}' is \"{text}\", which is not valid: {why}")),
        }
    }

    /// The source text as written in the model, for diagnostics.
    pub fn name(&self) -> String {
        match self {
            TimerSource::Literal(due) => due.to_string(),
            TimerSource::Variable { path, .. } => path.join("."),
        }
    }
}

/// How a JSON value reads in an incident message, or in a diagnostic about
/// a manifest entry that is not the shape it has to be.
pub(crate) fn describe(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "missing (or null)",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "a list",
        serde_json::Value::Object(_) => "a context",
    }
}

impl std::fmt::Display for TimerDue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TimerDue::Duration(s) | TimerDue::Date(s) | TimerDue::Cycle(s) => write!(f, "{s}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkKind {
    Service,
    User,
}

impl std::fmt::Display for WorkKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkKind::Service => write!(f, "service"),
            WorkKind::User => write!(f, "user"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExecKind {
    Start,
    End,
    TerminateEnd,
    Task {
        kind: WorkKind,
        topic: String,
    },
    /// Invokes a decision from the deployment's bundled DMN artifacts.
    ///
    /// The core never evaluates it: `step` parks here and emits
    /// `DecisionRequested`, the projection evaluates inside the same
    /// transaction, and the answer re-enters as `Command::CompleteDecision`.
    /// That is what keeps this crate free of dsntk *by construction*, and
    /// what lets a history replay without an evaluator at all — the recorded
    /// answer is command data, exactly like a handler's merge patch
    /// (`docs/dmn.md`, D3).
    BusinessRule {
        /// The invocable's name, resolved against the bundle at deploy.
        decision: String,
        /// Where the answer lands in the variable document, as a parsed FEEL
        /// qualified name (source text = `result.join(".")`).
        result: Vec<String>,
    },
    ExclusiveGateway {
        default_flow: Option<FlowIx>,
    },
    ParallelGateway,
    /// Interrupting error boundary: entered only when its host's work item
    /// raises a matching error code — never via a sequence flow.
    ErrorBoundary {
        code: String,
    },
    /// Timer intermediate catch: parks its token behind an armed timer.
    TimerCatch {
        due: TimerSource,
    },
    /// Message catch — an `intermediateCatchEvent` or a `receiveTask` (same
    /// semantics): parks its token behind a subscription. `key` is the
    /// parsed correlation qualified name (source text = `key.join(".")`).
    MessageCatch {
        message: String,
        key: Vec<String>,
    },
    /// Timer boundary: armed on the host's token, entered only by its timer
    /// firing — never via a sequence flow.
    TimerBoundary {
        due: TimerSource,
        interrupting: bool,
    },
    /// Message boundary: a subscription armed on the *host's* token, entered
    /// only by its own delivery — never via a sequence flow. `key` is the
    /// parsed correlation qualified name bound to the **boundary's** element
    /// id, never the host's.
    MessageBoundary {
        message: String,
        key: Vec<String>,
        interrupting: bool,
    },
    /// Parks its token and arms every target catch event; the first to fire
    /// wins and the rest are cancelled.
    EventBasedGateway,
    /// An embedded subprocess: entering allocates a runtime scope, spawns a
    /// token at `scope`'s start event, and parks the parent token here until
    /// that scope empties. Boundary events on it interrupt the whole scope.
    SubProcess {
        scope: ScopeIx,
    },
}

impl ExecKind {
    /// What this node subscribes with, if anything: `(message, correlation
    /// key)` for a message catch, a receive task **or** a message boundary.
    ///
    /// The one definition of "this is a message arm". `subscribe` is the
    /// single arming chokepoint for all three and `ambiguous-message-arm`
    /// reasons about exactly the same set, so a second `match` is a second
    /// place for the two to disagree about what an arm is.
    pub fn message_arm(&self) -> Option<(&str, &[String])> {
        match self {
            ExecKind::MessageCatch { message, key }
            | ExecKind::MessageBoundary { message, key, .. } => {
                Some((message.as_str(), key.as_slice()))
            }
            _ => None,
        }
    }

    /// Does triggering this boundary cancel its host?
    ///
    /// `cancelActivity` for a timer or a message boundary, straight from the
    /// model. An error boundary is always interrupting — the activity that
    /// raised the error has already ended — and lint refuses the XML that
    /// says otherwise. Anything else answers `true` because nothing else is
    /// ever asked: the callers are the two arm paths in `step`, which reach
    /// this only for a boundary that just fired.
    pub fn boundary_interrupts(&self) -> bool {
        match self {
            ExecKind::TimerBoundary { interrupting, .. }
            | ExecKind::MessageBoundary { interrupting, .. } => *interrupting,
            _ => true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExecNode {
    pub id: String,
    pub kind: ExecKind,
    pub incoming: Vec<FlowIx>,
    pub outgoing: Vec<FlowIx>,
}

#[derive(Debug, Clone)]
pub struct ExecFlow {
    pub id: String,
    pub source: NodeIx,
    pub target: NodeIx,
    pub condition: Option<condition::Expr>,
}

#[derive(Debug, Clone)]
pub struct ExecutableProcess {
    pub process_id: String,
    nodes: Vec<ExecNode>,
    flows: Vec<ExecFlow>,
    ids: BTreeMap<String, NodeIx>,
    /// host node -> (error code, boundary node)
    error_boundaries: BTreeMap<NodeIx, Vec<(String, NodeIx)>>,
    /// host node -> the boundary nodes armed on the host's token whenever it
    /// starts waiting — timer *and* message, in **XML declaration order**.
    /// One list rather than one per kind because arming allocates timer and
    /// subscription ids and the golden traces pin them: two lists would make
    /// the trace depend on which kind the code happened to walk first.
    /// `error_boundaries` stays separate — it is matched by code, never armed.
    boundaries: BTreeMap<NodeIx, Vec<NodeIx>>,
    /// Each static scope's start event; index 0 is the process root. The
    /// rest of the scope tree (parents, owners) is only needed while
    /// compiling, so it does not survive into the runtime model.
    scope_starts: Vec<NodeIx>,
    start: NodeIx,
}

/// One `ambiguous-message-arm` group: the arms that catch `message` under
/// `binding` and are live over the same span, in declaration order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmbiguousArms {
    pub elements: Vec<String>,
    pub message: String,
    pub binding: String,
}

impl AmbiguousArms {
    /// One group as a phrase, for the log line a deploy failure leaves
    /// behind; [`crate::check`] renders the same group as a diagnostic per
    /// element for an editor to highlight.
    pub fn describe(&self) -> String {
        format!(
            "{} catch '{}' correlated by '{}'",
            and_list(&self.elements),
            self.message,
            self.binding
        )
    }
}

/// `'a' and 'b'`, `'a', 'b' and 'c'` — quoted, and readable at any length.
/// Shared with [`crate::check`] so a group reads the same in the error and
/// in the diagnostic.
pub(crate) fn and_list(elements: &[String]) -> String {
    match elements {
        [] => String::new(),
        [only] => format!("'{only}'"),
        [rest @ .., last] => format!(
            "{} and '{last}'",
            rest.iter()
                .map(|e| format!("'{e}'"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CompileError {
    #[error("model rejected by the linter ({} error diagnostics)", .0.len())]
    RejectedByLint(Vec<Diagnostic>),
    #[error("no process with id '{0}' in the definitions")]
    UnknownProcess(String),
    #[error("'{element}' ({what}) is not executable yet — {phase}")]
    NotYetExecutable {
        element: String,
        what: String,
        phase: &'static str,
    },
    /// `message-has-correlation`: every message catch element must be bound
    /// to a correlation key via `Bindings::correlation` — there is no
    /// default, exactly like an unresolved topic fails a deploy.
    #[error(
        "message element(s) without a correlation binding: {} — bind each \
         with Bindings::correlation(element_id, feel_qualified_name)",
        .0.join(", ")
    )]
    MissingCorrelation(Vec<String>),
    #[error("correlation binding on '{element}': {reason}")]
    InvalidCorrelation { element: String, reason: String },
    /// `ambiguous-message-arm`: arms for the same message *and* the same
    /// correlation binding that are live at the same time. The runtime rule
    /// (a second open `(message, key)` freezes the instance) stays the
    /// backstop; these shapes are certain the moment the manifest is known,
    /// and a certain freeze belongs at deploy rather than in an incident.
    ///
    /// **Every** group, not the first: a model with two ambiguous hosts is
    /// two things to fix, and a modeller who only hears about one fixes it
    /// and gets refused again — the same reason `MissingCorrelation` carries
    /// every element.
    #[error(
        "message arms that are live at the same time, so every delivery would be \
         ambiguous: {}",
        .0.iter().map(AmbiguousArms::describe).collect::<Vec<_>>().join("; ")
    )]
    AmbiguousMessageArm(Vec<AmbiguousArms>),
    /// `decision-has-binding`: a business-rule task says *that* a decision
    /// happens, never which — that is manifest data, exactly like a topic.
    /// Unlike a topic there is no sensible default: guessing a decision by
    /// element id would invoke business logic nobody chose.
    #[error(
        "business-rule task(s) without a decision binding: {} — bind each with \
         Bindings::decision(element_id, decision_name, result_path)",
        .0.join(", ")
    )]
    MissingDecision(Vec<String>),
    #[error("decision binding on '{element}': {reason}")]
    InvalidDecision { element: String, reason: String },
    #[error("internal: {0} (lint should have prevented this)")]
    Internal(String),
}

impl ExecutableProcess {
    pub fn compile(
        defs: &Definitions,
        process_id: &str,
        bindings: &Bindings,
    ) -> Result<Self, CompileError> {
        let diagnostics = rbpmn_model::lint(defs);
        if has_errors(&diagnostics) {
            return Err(CompileError::RejectedByLint(
                diagnostics
                    .into_iter()
                    .filter(|d| d.severity == rbpmn_model::Severity::Error)
                    .collect(),
            ));
        }
        Self::compile_unlinted(defs, process_id, bindings)
    }

    /// Compile **without the deploy-time lint gate**, for models the linter
    /// would refuse.
    ///
    /// This exists for exactly one purpose: proving the structural rules earn
    /// their keep. Running a rejected model shows the concrete hazard the rule
    /// prevents — a parallel join collecting a second token on one flow, a
    /// starved join, a token stuck forever — instead of leaving the
    /// restriction as an assertion nobody has tested
    /// (`crates/rbpmn-core/tests/mutation.rs`, docs/stress-testing.md §3d).
    ///
    /// Never reachable from a normal build: it is behind a non-default feature
    /// that only this crate's own tests enable. The semantic core assumes
    /// lint-clean input, so anything here may return
    /// [`CompileError::Internal`], hit [`crate::StepError::Invariant`], or
    /// simply misbehave — that is the point.
    #[cfg(feature = "unlinted-compile")]
    pub fn compile_without_lint(
        defs: &Definitions,
        process_id: &str,
        bindings: &Bindings,
    ) -> Result<Self, CompileError> {
        Self::compile_unlinted(defs, process_id, bindings)
    }

    fn compile_unlinted(
        defs: &Definitions,
        process_id: &str,
        bindings: &Bindings,
    ) -> Result<Self, CompileError> {
        let process = defs
            .processes
            .iter()
            .find(|p| p.id == process_id)
            .ok_or_else(|| CompileError::UnknownProcess(process_id.to_string()))?;

        let scope = &process.body;
        let not_yet = |node: &FlowNode, phase: &'static str| CompileError::NotYetExecutable {
            element: node.id.clone(),
            what: node.kind.describe().to_string(),
            phase,
        };

        // The message name lives in the XML (`messageRef` -> named message);
        // the correlation key lives in the bindings manifest. Both resolve
        // here, so a definition that compiles is fully wired.
        let message_name =
            |node: &FlowNode, message_ref: &Option<Id>| -> Result<String, CompileError> {
                message_ref
                    .as_deref()
                    .and_then(|r| defs.messages.iter().find(|m| m.id == r))
                    .and_then(|m| m.name.clone())
                    .ok_or_else(|| {
                        CompileError::Internal(format!(
                            "message element '{}' without a named message survived lint",
                            node.id
                        ))
                    })
            };
        let mut missing_correlations: Vec<String> = Vec::new();
        let mut missing_decisions: Vec<String> = Vec::new();
        let mut correlation = |node: &FlowNode| -> Result<Vec<String>, CompileError> {
            let Some(name) = bindings.correlations.get(&node.id) else {
                missing_correlations.push(node.id.clone());
                return Ok(Vec::new()); // placeholder; rejected below
            };
            condition::parse_qname(name).map_err(|e| CompileError::InvalidCorrelation {
                element: node.id.clone(),
                reason: e.to_string(),
            })
        };
        // Literal first, always — the same order the linter used to accept
        // this element. The `xsi:type="bpmn:tFormalExpression"` marker is
        // deliberately *not* consulted: bpmn-moddle writes it for any
        // expression object, so every bpmn-js modeler stamps it on ordinary
        // literal durations, and keying off it turned `P3D` into a variable
        // named `P3D`.
        let timer_due = |node: &FlowNode, spec: &TimerSpec| -> Result<TimerSource, CompileError> {
            let from_variable = |kind: TimerKind, text: &str| {
                rbpmn_model::condition::parse_qname(text)
                    .map(|path| TimerSource::Variable { kind, path })
                    .map_err(|e| {
                        CompileError::Internal(format!(
                            "timer '{}' is neither an ISO-8601 literal nor a qualified \
                             name ({e}) — lint should have rejected it",
                            node.id
                        ))
                    })
            };
            let kind = match spec {
                TimerSpec::Duration(_) => TimerKind::Duration,
                TimerSpec::Date(_) => TimerKind::Date,
                // The one place a cycle is admitted, asking the same
                // predicate lint accepted the element with: everywhere else
                // the first occurrence ends the wait, so a cycle that got
                // this far is a linter that stopped agreeing with the model.
                // Guarding here rather than at each call site is the point —
                // every armed timer resolves through `timer_due`, so there is
                // exactly one place to keep in step.
                TimerSpec::Cycle(_) if !node.kind.executes_cycle() => {
                    return Err(CompileError::Internal(format!(
                        "timer '{}' with a cycle where it cannot repeat survived lint",
                        node.id
                    )));
                }
                TimerSpec::Cycle(_) => TimerKind::Cycle,
                TimerSpec::Missing => {
                    return Err(CompileError::Internal(format!(
                        "timer '{}' with a missing definition survived lint",
                        node.id
                    )));
                }
            };
            // Literal first, by the same table lint consulted to accept this
            // element — so the two cannot disagree about what is a literal
            // and what is a variable name.
            let (_, text, literal) = spec
                .literal_check()
                .expect("a missing definition returned above");
            if literal.is_ok() {
                Ok(TimerSource::Literal(kind.due(text)))
            } else {
                from_variable(kind, text)
            }
        };

        // Flatten the scope tree into one node/flow array, remembering which
        // scope each node came from. Ids are unique across scopes (the
        // linter enforces it), so one global id map still resolves flows.
        let mut scope_bodies: Vec<&FlowScope> = vec![scope];
        let mut flat: Vec<(ScopeIx, &FlowNode)> = Vec::new();
        let mut child_scope: BTreeMap<NodeIx, ScopeIx> = BTreeMap::new();
        let mut si = 0;
        while si < scope_bodies.len() {
            for node in &scope_bodies[si].nodes {
                let flat_ix = flat.len();
                flat.push((si, node));
                if let NodeKind::SubProcess(sp) = &node.kind {
                    if sp.triggered_by_event {
                        return Err(not_yet(node, "event subprocesses arrive in v3"));
                    }
                    child_scope.insert(flat_ix, scope_bodies.len());
                    scope_bodies.push(&sp.body);
                }
            }
            si += 1;
        }

        let mut nodes: Vec<ExecNode> = Vec::with_capacity(flat.len());
        let mut scope_starts: Vec<Option<NodeIx>> = vec![None; scope_bodies.len()];
        let mut default_flow_ids: Vec<(NodeIx, &str)> = Vec::new();
        let mut node_ix: BTreeMap<&str, NodeIx> = BTreeMap::new();
        let mut boundary_hosts: Vec<(NodeIx, String)> = Vec::new();
        for (ix, (owning_scope, node)) in flat.iter().enumerate() {
            let kind = match &node.kind {
                NodeKind::Start(StartTrigger::None) => ExecKind::Start,
                NodeKind::End(EndKind::None) => ExecKind::End,
                NodeKind::End(EndKind::Terminate) => ExecKind::TerminateEnd,
                NodeKind::ServiceTask { .. } => ExecKind::Task {
                    kind: WorkKind::Service,
                    topic: bindings
                        .topics
                        .get(&node.id)
                        .cloned()
                        .unwrap_or_else(|| node.id.clone()),
                },
                NodeKind::BusinessRuleTask => {
                    let Some(binding) = bindings.decisions.get(&node.id) else {
                        missing_decisions.push(node.id.clone());
                        continue;
                    };
                    let result = match rbpmn_model::condition::parse_qname(&binding.result) {
                        Ok(path) => path,
                        Err(e) => {
                            return Err(CompileError::InvalidDecision {
                                element: node.id.clone(),
                                reason: e.to_string(),
                            });
                        }
                    };
                    ExecKind::BusinessRule {
                        decision: binding.decision.clone(),
                        result,
                    }
                }
                NodeKind::UserTask => ExecKind::Task {
                    kind: WorkKind::User,
                    topic: bindings
                        .topics
                        .get(&node.id)
                        .cloned()
                        .unwrap_or_else(|| node.id.clone()),
                },
                NodeKind::ExclusiveGateway { default_flow } => {
                    // Resolved once flows exist; recorded here so nothing
                    // has to rescan the flattened node array later.
                    if let Some(id) = default_flow {
                        default_flow_ids.push((ix, id.as_str()));
                    }
                    ExecKind::ExclusiveGateway { default_flow: None }
                }
                NodeKind::ParallelGateway => ExecKind::ParallelGateway,
                NodeKind::EventBasedGateway => ExecKind::EventBasedGateway,
                NodeKind::Catch(CatchTrigger::Timer(spec)) => ExecKind::TimerCatch {
                    due: timer_due(node, spec)?,
                },
                NodeKind::Catch(CatchTrigger::Message(message_ref)) => ExecKind::MessageCatch {
                    message: message_name(node, message_ref)?,
                    key: correlation(node)?,
                },
                NodeKind::ReceiveTask { message_ref } => {
                    // A receive task IS a message catch (identical semantics,
                    // task-shaped notation) — it never creates a work item.
                    ExecKind::MessageCatch {
                        message: message_name(node, message_ref)?,
                        key: correlation(node)?,
                    }
                }
                NodeKind::Start(StartTrigger::Message(_))
                | NodeKind::End(EndKind::Message(_))
                | NodeKind::Throw(_) => {
                    return Err(not_yet(
                        node,
                        "message start/throw (cross-definition messaging) is not \
                         part of phase 3 — external systems deliver via correlate()",
                    ));
                }
                NodeKind::Catch(CatchTrigger::Unsupported { .. }) => {
                    return Err(CompileError::Internal(format!(
                        "unsupported catch trigger on '{}' survived lint",
                        node.id
                    )));
                }
                NodeKind::Boundary(b) => {
                    // Recorded once, before the trigger says which kind of
                    // arm this is: every boundary has a host, and the host
                    // pass below is the one place that validates it.
                    boundary_hosts.push((ix, b.attached_to.clone().unwrap_or_default()));
                    match &b.trigger {
                        BoundaryTrigger::Error { error_ref } => {
                            let code = error_ref
                                .as_deref()
                                .and_then(|r| defs.errors.iter().find(|e| e.id == r))
                                .and_then(|e| e.code.clone())
                                .ok_or_else(|| {
                                    CompileError::Internal(format!(
                                        "error boundary '{}' without a coded error survived lint",
                                        node.id
                                    ))
                                })?;
                            ExecKind::ErrorBoundary { code }
                        }
                        // `cancelActivity` for both kinds, and lint is what
                        // makes reading it safe: only a timer or a message
                        // boundary may be non-interrupting, an error boundary
                        // never is. Whether *this* timer may repeat is
                        // `timer_due`'s single guard, not a second one here.
                        BoundaryTrigger::Timer(spec) => ExecKind::TimerBoundary {
                            due: timer_due(node, spec)?,
                            interrupting: b.cancel_activity,
                        },
                        // The correlation binding is the *boundary's* own element
                        // id, exactly as a catch's is its own: the XML says which
                        // message is caught here, the manifest says by which key.
                        BoundaryTrigger::Message(message_ref) => ExecKind::MessageBoundary {
                            message: message_name(node, message_ref)?,
                            key: correlation(node)?,
                            interrupting: b.cancel_activity,
                        },
                        _ => {
                            return Err(CompileError::Internal(format!(
                                "unsupported boundary trigger on '{}' survived lint",
                                node.id
                            )));
                        }
                    }
                }
                NodeKind::SubProcess(_) => ExecKind::SubProcess {
                    scope: child_scope[&ix],
                },
                other => {
                    return Err(CompileError::Internal(format!(
                        "unexpected {} '{}' survived lint",
                        other.describe(),
                        node.id
                    )));
                }
            };
            node_ix.insert(node.id.as_str(), ix);
            if kind == ExecKind::Start {
                scope_starts[*owning_scope] = Some(ix);
            }
            nodes.push(ExecNode {
                id: node.id.clone(),
                kind,
                incoming: Vec::new(),
                outgoing: Vec::new(),
            });
        }
        if !missing_decisions.is_empty() {
            return Err(CompileError::MissingDecision(missing_decisions));
        }
        if !missing_correlations.is_empty() {
            return Err(CompileError::MissingCorrelation(missing_correlations));
        }

        let mut flows: Vec<ExecFlow> = Vec::new();
        for body in &scope_bodies {
            for flow in &body.flows {
                let fi = flows.len();
                let source = *node_ix.get(flow.source.as_str()).ok_or_else(|| {
                    CompileError::Internal(format!("dangling flow '{}'", flow.id))
                })?;
                let target = *node_ix.get(flow.target.as_str()).ok_or_else(|| {
                    CompileError::Internal(format!("dangling flow '{}'", flow.id))
                })?;
                let cond = flow
                    .condition
                    .as_deref()
                    .map(condition::parse)
                    .transpose()
                    .map_err(|e| {
                        CompileError::Internal(format!("condition on '{}': {e}", flow.id))
                    })?;
                nodes[source].outgoing.push(fi);
                nodes[target].incoming.push(fi);
                flows.push(ExecFlow {
                    id: flow.id.clone(),
                    source,
                    target,
                    condition: cond,
                });
            }
        }

        let flow_by_id: BTreeMap<&str, FlowIx> = flows
            .iter()
            .enumerate()
            .map(|(fi, f)| (f.id.as_str(), fi))
            .collect();
        for (gateway, flow_id) in default_flow_ids {
            if let ExecKind::ExclusiveGateway { default_flow } = &mut nodes[gateway].kind {
                *default_flow = flow_by_id.get(flow_id).copied();
            }
        }

        let mut error_boundaries: BTreeMap<NodeIx, Vec<(String, NodeIx)>> = BTreeMap::new();
        let mut boundaries: BTreeMap<NodeIx, Vec<NodeIx>> = BTreeMap::new();
        for (boundary_ix, host_id) in boundary_hosts {
            let host = *node_ix.get(host_id.as_str()).ok_or_else(|| {
                CompileError::Internal(format!("boundary host '{host_id}' missing"))
            })?;
            match &nodes[boundary_ix].kind {
                ExecKind::ErrorBoundary { code } => {
                    // Errors originate from failing service tasks, and
                    // propagate outward to the nearest enclosing scope whose
                    // subprocess carries a matching boundary — the scoped
                    // error handler embedded subprocesses exist for.
                    if !matches!(
                        nodes[host].kind,
                        ExecKind::Task {
                            kind: WorkKind::Service,
                            ..
                        } | ExecKind::SubProcess { .. }
                    ) {
                        return Err(CompileError::Internal(format!(
                            "error boundary '{}' on unsupported host survived lint",
                            nodes[boundary_ix].id
                        )));
                    }
                    error_boundaries
                        .entry(host)
                        .or_default()
                        .push((code.clone(), boundary_ix));
                }
                // Timer and message boundaries arm on any waiting host token:
                // tasks (work items), receive tasks (subscriptions), and
                // subprocesses (the whole scope). Never a business-rule task
                // — its token is answered inside the transaction that parked
                // it, so the arm could only ever be created and withdrawn in
                // one step (lint refuses it; this is the "survived lint"
                // guard behind that).
                //
                // Asked of the **model** kind, which is the kind lint judged:
                // a second allowlist over `ExecKind` is a second answer to
                // one question, and this one had already drifted (an
                // intermediate message catch and a receive task are both
                // `MessageCatch`, so it accepted a host lint refuses).
                // `nodes` and `flat` share indices here — the only `continue`
                // in the node pass returned above with `MissingDecision`.
                ExecKind::TimerBoundary { .. } | ExecKind::MessageBoundary { .. } => {
                    if !flat[host].1.kind.is_supported_boundary_host() {
                        return Err(CompileError::Internal(format!(
                            "boundary '{}' on unsupported host survived lint",
                            nodes[boundary_ix].id
                        )));
                    }
                    boundaries.entry(host).or_default().push(boundary_ix);
                }
                _ => unreachable!("boundary_hosts only collects boundary nodes"),
            }
        }

        // Node indices line up with `flat` here: the one `continue` in the
        // node pass (a business-rule task without a binding) already returned
        // above with `MissingDecision`.
        let owning_scope: Vec<ScopeIx> = flat.iter().map(|(s, _)| *s).collect();
        if let Some(e) =
            ambiguous_message_arm(&nodes, &flows, &boundaries, &owning_scope, &child_scope)
        {
            return Err(e);
        }

        // Each scope has exactly one start event (`single-start-event`),
        // recorded during the node pass above rather than rescanned here.
        let scope_starts: Vec<NodeIx> = scope_starts
            .into_iter()
            .enumerate()
            .map(|(s, start)| {
                start.ok_or_else(|| CompileError::Internal(format!("scope {s} has no start event")))
            })
            .collect::<Result<_, _>>()?;
        let start = scope_starts[0];

        let ids = nodes
            .iter()
            .enumerate()
            .map(|(ix, n)| (n.id.clone(), ix))
            .collect();
        Ok(ExecutableProcess {
            process_id: process.id.clone(),
            nodes,
            flows,
            ids,
            error_boundaries,
            boundaries,
            scope_starts,
            start,
        })
    }

    pub fn start(&self) -> NodeIx {
        self.start
    }

    pub fn node(&self, ix: NodeIx) -> &ExecNode {
        &self.nodes[ix]
    }

    pub fn flow(&self, ix: FlowIx) -> &ExecFlow {
        &self.flows[ix]
    }

    pub fn node_id(&self, ix: NodeIx) -> &str {
        &self.nodes[ix].id
    }

    pub fn node_by_id(&self, id: &str) -> Option<NodeIx> {
        self.ids.get(id).copied()
    }

    /// The error boundary on `host` matching `code` exactly, if any.
    pub fn error_boundary(&self, host: NodeIx, code: &str) -> Option<NodeIx> {
        self.error_boundaries
            .get(&host)?
            .iter()
            .find(|(c, _)| c == code)
            .map(|(_, b)| *b)
    }

    /// Where execution begins inside a static scope.
    pub fn scope_start(&self, scope: ScopeIx) -> NodeIx {
        self.scope_starts[scope]
    }

    /// [`Self::scope_start`] for callers enumerating scopes they did not
    /// count (the state-space explorer roots its walk at every scope).
    pub fn try_scope_start(&self, scope: ScopeIx) -> Option<NodeIx> {
        self.scope_starts.get(scope).copied()
    }

    /// The boundaries armed whenever `host` starts waiting — timer and
    /// message alike, in XML declaration order. Error boundaries are not
    /// here: they are matched by code when the host fails, never armed.
    pub fn boundaries(&self, host: NodeIx) -> &[NodeIx] {
        self.boundaries.get(&host).map(Vec::as_slice).unwrap_or(&[])
    }

    /// What `node` subscribes with, if anything — see
    /// [`ExecKind::message_arm`], which is where the answer lives.
    pub fn message_arm(&self, node: NodeIx) -> Option<(&str, &[String])> {
        self.nodes[node].kind.message_arm()
    }

    pub fn flow_by_id(&self, id: &str) -> Option<FlowIx> {
        self.flows.iter().position(|f| f.id == id)
    }

    /// (element id, resolved topic) of every service task — what the
    /// `unresolved-topic` deploy check compares against the environment.
    pub fn service_topics(&self) -> impl Iterator<Item = (&str, &str)> {
        self.nodes.iter().filter_map(|n| match &n.kind {
            ExecKind::Task {
                kind: WorkKind::Service,
                topic,
            } => Some((n.id.as_str(), topic.as_str())),
            _ => None,
        })
    }
}

/// `ambiguous-message-arm` (docs/design/boundary-messages.md §2.4).
///
/// Four shapes are certain the moment the manifest is known: two message
/// boundaries on one host, a message boundary on a receive task catching the
/// host's own message, a message boundary on a subprocess with a catch of
/// the same message anywhere inside its body, and a **non-interrupting**
/// message boundary whose own side path carries an arm for the same pair.
/// Certain because those arms are live over exactly the same span — the
/// host's wait — so *every* delivery would be ambiguous, not merely some
/// interleaving of them. The runtime duplicate rule (a second open
/// `(message, key)` freezes the instance) stays the backstop for everything
/// else.
///
/// The side path is certain for a reason of its own, and it is the *first*
/// delivery that freezes: a non-interrupting boundary re-arms itself and
/// then spawns the side token **in the same step**, so the new arm is
/// already open when the side token reaches the catch. The host is untouched
/// and still parked, so its own arm and its other boundaries' arms are open
/// too — which is why the side path joins the host's group rather than
/// forming one of its own. `side-path-message-arm` warns about the *other*
/// half of the same shape (an arm colliding with an earlier activation's,
/// which only the manifest's key can rule out); this refuses the half that
/// cannot come out any other way.
///
/// The same message with a **different** binding is accepted: the two resolve
/// to different keys and both may legitimately be live. That is the whole
/// reason this is an L2 rule and not a linter one — only the manifest knows,
/// and the manifest is never in the XML.
fn ambiguous_message_arm(
    nodes: &[ExecNode],
    flows: &[ExecFlow],
    boundaries: &BTreeMap<NodeIx, Vec<NodeIx>>,
    owning_scope: &[ScopeIx],
    child_scope: &BTreeMap<NodeIx, ScopeIx>,
) -> Option<CompileError> {
    // scope -> the scope its owning subprocess sits in; the root has none.
    let scope_count = child_scope.values().copied().max().map_or(1, |m| m + 1);
    let mut parent_scope: Vec<Option<ScopeIx>> = vec![None; scope_count];
    for (&owner, &child) in child_scope {
        parent_scope[child] = Some(owning_scope[owner]);
    }
    let inside = |scope: ScopeIx, root: ScopeIx| -> bool {
        let mut at = scope;
        loop {
            if at == root {
                return true;
            }
            match parent_scope[at] {
                Some(p) => at = p,
                None => return false,
            }
        }
    };
    // Everything inside a subprocess's body, at any depth — the scope-parent
    // chain, so one subprocess covers its whole subtree.
    let body_of = |activity: NodeIx| -> Vec<NodeIx> {
        child_scope.get(&activity).map_or_else(Vec::new, |&body| {
            (0..nodes.len())
                .filter(|&n| inside(owning_scope[n], body))
                .collect()
        })
    };
    // The forward closure from a non-interrupting boundary: its side path
    // over sequence flows, the boundaries attached to activities on it, and
    // the bodies of the subprocesses on it. The same set `boundary-side-path`
    // reasons about in the linter, computed here over the compiled graph —
    // and it must include the subprocess bodies, because "put it in a
    // subprocess" is a repair that rule itself recommends.
    let side_path = |seed: NodeIx| -> Vec<NodeIx> {
        let mut seen = vec![false; nodes.len()];
        let mut queue = vec![seed];
        seen[seed] = true;
        let mut reached = Vec::new();
        while let Some(v) = queue.pop() {
            reached.push(v);
            let onward = nodes[v]
                .outgoing
                .iter()
                .map(|&fi| flows[fi].target)
                .chain(boundaries.get(&v).into_iter().flatten().copied())
                .chain(body_of(v));
            for w in onward {
                if !seen[w] {
                    seen[w] = true;
                    queue.push(w);
                }
            }
        }
        reached
    };
    let mut found: Vec<AmbiguousArms> = Vec::new();
    for (&host, attached) in boundaries {
        let mut live: Vec<NodeIx> = attached
            .iter()
            .copied()
            .filter(|&b| matches!(nodes[b].kind, ExecKind::MessageBoundary { .. }))
            .collect();
        if live.is_empty() {
            continue;
        }
        // A receive task's own arm is live for exactly as long as its
        // boundaries are — that is what makes host-vs-boundary certain.
        if matches!(nodes[host].kind, ExecKind::MessageCatch { .. }) {
            live.push(host);
        }
        // A subprocess boundary is armed before the body starts and withdrawn
        // when it ends, so it overlaps every arm inside, at any depth.
        live.extend(body_of(host));
        // A non-interrupting message boundary re-arms and *then* spawns the
        // side token, so its next arm is open while the side path runs beside
        // the still-parked host: every arm on that path is live with the
        // host's own.
        let side_arms: Vec<NodeIx> = attached
            .iter()
            .copied()
            .filter(|&b| {
                matches!(
                    nodes[b].kind,
                    ExecKind::MessageBoundary {
                        interrupting: false,
                        ..
                    }
                )
            })
            .flat_map(&side_path)
            .collect();
        live.extend(side_arms);
        live.sort_unstable();
        live.dedup();

        let mut groups: BTreeMap<(String, String), Vec<NodeIx>> = BTreeMap::new();
        for n in live {
            if let Some((message, key)) = nodes[n].kind.message_arm() {
                groups
                    .entry((message.to_string(), key.join(".")))
                    .or_default()
                    .push(n);
            }
        }
        for ((message, binding), elements) in groups {
            if elements.len() < 2 {
                continue;
            }
            let group = AmbiguousArms {
                elements: elements.iter().map(|&n| nodes[n].id.clone()).collect(),
                message,
                binding,
            };
            // Nested hosts can reach the same set twice (a subprocess
            // boundary sees an inner receive task's own boundary, and so
            // does that receive task): one group, reported once.
            if !found.contains(&group) {
                found.push(group);
            }
        }
    }
    (!found.is_empty()).then_some(CompileError::AmbiguousMessageArm(found))
}

#[cfg(test)]
mod index_declaration_tests {
    use super::*;

    fn indexes(json: &str) -> Bindings {
        serde_json::from_str(json).expect("manifest parses")
    }

    /// The back-compat contract, and the reason it is not cosmetic: `deploy`
    /// hashes the serialized manifest into `content_hash`, so a manifest of
    /// definition-scoped entries must serialize to the *same bytes* it always
    /// has or every existing deployment allocates a new version on its next
    /// redeploy.
    #[test]
    fn definition_scoped_manifests_serialize_byte_for_byte() {
        let b = Bindings::new().index("channel").index("region");
        assert_eq!(
            serde_json::to_string(&b).unwrap(),
            r#"{"topics":{},"correlations":{},"indexes":["channel","region"],"decisions":{}}"#
        );
    }

    #[test]
    fn a_bare_string_is_definition_scoped() {
        let b = indexes(r#"{"indexes":["channel"]}"#);
        assert_eq!(
            b.indexes.iter().collect::<Vec<_>>(),
            vec![&IndexDeclaration::definition("channel")]
        );
    }

    /// The long spelling of the default normalizes to the short one, so the
    /// same wiring hashes the same however it was written.
    #[test]
    fn the_explicit_definition_scope_normalizes_to_the_string_form() {
        let long = indexes(r#"{"indexes":[{"field":"channel","scope":"definition"}]}"#);
        let short = indexes(r#"{"indexes":["channel"]}"#);
        assert_eq!(long, short);
        assert_eq!(
            serde_json::to_string(&long).unwrap(),
            serde_json::to_string(&short).unwrap()
        );
    }

    #[test]
    fn shared_round_trips_as_an_object() {
        let b = Bindings::new().shared_index("order_no");
        let json = serde_json::to_string(&b).unwrap();
        assert!(
            json.contains(r#""indexes":[{"field":"order_no","scope":"shared"}]"#),
            "{json}"
        );
        assert_eq!(indexes(&json), b);
    }

    /// Both spellings of one field are two distinct declarations — the set
    /// does not collapse them, and deploy refuses the contradiction (that
    /// check lives in the engine, which is where the scopes turn into SQL).
    #[test]
    fn the_two_scopes_of_one_field_are_distinct_entries() {
        let b = indexes(r#"{"indexes":["f",{"field":"f","scope":"shared"}]}"#);
        assert_eq!(b.indexes.len(), 2);
    }

    #[test]
    fn an_unknown_scope_is_refused_and_names_the_valid_ones() {
        let e =
            serde_json::from_str::<Bindings>(r#"{"indexes":[{"field":"f","scope":"sharded"}]}"#)
                .unwrap_err()
                .to_string();
        assert!(e.contains("definition") && e.contains("shared"), "{e}");
    }

    #[test]
    fn an_unknown_key_in_the_object_form_is_refused() {
        let e = serde_json::from_str::<Bindings>(r#"{"indexes":[{"field":"f","scoop":"shared"}]}"#)
            .unwrap_err()
            .to_string();
        assert!(e.contains("scoop"), "{e}");
    }

    #[test]
    fn the_object_form_requires_a_field() {
        let e = serde_json::from_str::<Bindings>(r#"{"indexes":[{"scope":"shared"}]}"#)
            .unwrap_err()
            .to_string();
        assert!(e.contains("field"), "{e}");
    }

    /// Field first, so a definition-only manifest keeps the plain
    /// lexicographic order it has always had.
    #[test]
    fn entries_order_by_field() {
        let b = Bindings::new()
            .index("zulu")
            .shared_index("alpha")
            .index("mike");
        assert_eq!(
            b.indexes
                .iter()
                .map(|i| i.field.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "mike", "zulu"]
        );
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;
    use serde_json::json;

    /// The hash contract, stated from the config side: an empty group must
    /// not appear in the serialized manifest, or every definition in every
    /// installation gets a new version on the first redeploy after the
    /// upgrade that added it. `definition_scoped_manifests_serialize_byte_for_byte`
    /// is the other half — this one names why the attribute is there.
    #[test]
    fn an_empty_config_group_does_not_reach_the_hashed_manifest() {
        let json = serde_json::to_string(&Bindings::new()).unwrap();
        assert!(!json.contains("config"), "{json}");
    }

    #[test]
    fn config_round_trips() {
        let b = Bindings::new().config("send_warning", json!({"template": "warning_first"}));
        let json = serde_json::to_string(&b).unwrap();
        assert!(
            json.contains(r#""config":{"send_warning":{"template":"warning_first"}}"#),
            "{json}"
        );
        assert_eq!(serde_json::from_str::<Bindings>(&json).unwrap(), b);
    }

    /// Free JSON, and *free* means the nesting too: rbpmn never looks inside,
    /// so nothing here may depend on the shape below the top level.
    #[test]
    fn config_values_are_not_interpreted() {
        let value = json!({"letters": ["a", {"b": [1, 2, null]}], "n": 1.5});
        let b = Bindings::new().config("st", value.clone());
        let back: Bindings = serde_json::from_str(&serde_json::to_string(&b).unwrap()).unwrap();
        assert_eq!(back.config["st"], value);
    }
}
