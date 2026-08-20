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
/// work-item topic, and message element -> correlation key (a FEEL qualified
/// name into the instance variables). Unmapped tasks default to their
/// element id; correlations have **no default** — every message catch must
/// be mapped or compilation fails (`message-has-correlation`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Bindings {
    #[serde(default)]
    pub topics: BTreeMap<String, String>,
    #[serde(default)]
    pub correlations: BTreeMap<String, String>,
    /// Variables fields the application filters/counts tasks by — the
    /// engine creates a partial expression index per entry at deploy
    /// (`declare_index`). Entirely optional performance declarations.
    #[serde(default)]
    pub indexes: std::collections::BTreeSet<String>,
    /// Business-rule task -> the decision it invokes. This is the spot where
    /// every other engine writes `camunda:decisionRef` into the XML; here it
    /// is manifest data, versioned with the definition and reviewable in git
    /// next to it (`docs/dmn.md`, D5).
    #[serde(default)]
    pub decisions: BTreeMap<String, DecisionBinding>,
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

    /// Declare a filterable variables field (optional, performance only).
    pub fn index(mut self, field: impl Into<String>) -> Self {
        self.indexes.insert(field.into());
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
}

/// Which ISO-8601 shape a variable-sourced timer must resolve to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TimerKind {
    Duration,
    Date,
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
        let checked = match kind {
            TimerKind::Duration => rbpmn_model::iso8601::validate_duration(text),
            TimerKind::Date => rbpmn_model::iso8601::validate_datetime(text),
        };
        match checked {
            Ok(()) => Ok(match kind {
                TimerKind::Duration => TimerDue::Duration(text.clone()),
                TimerKind::Date => TimerDue::Date(text.clone()),
            }),
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

/// How a JSON value reads in an incident message.
fn describe(value: &serde_json::Value) -> &'static str {
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
            TimerDue::Duration(s) | TimerDue::Date(s) => write!(f, "{s}"),
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
    /// Interrupting timer boundary: armed on the host's token, entered only
    /// by its timer firing — never via a sequence flow.
    TimerBoundary {
        due: TimerSource,
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
    /// host node -> its interrupting timer boundary nodes, armed on the
    /// host's token whenever the host starts waiting.
    timer_boundaries: BTreeMap<NodeIx, Vec<NodeIx>>,
    /// Each static scope's start event; index 0 is the process root. The
    /// rest of the scope tree (parents, owners) is only needed while
    /// compiling, so it does not survive into the runtime model.
    scope_starts: Vec<NodeIx>,
    start: NodeIx,
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
            match spec {
                TimerSpec::Duration(s) => {
                    if rbpmn_model::iso8601::validate_duration(s).is_ok() {
                        Ok(TimerSource::Literal(TimerDue::Duration(s.clone())))
                    } else {
                        from_variable(TimerKind::Duration, s)
                    }
                }
                TimerSpec::Date(s) => {
                    if rbpmn_model::iso8601::validate_datetime(s).is_ok() {
                        Ok(TimerSource::Literal(TimerDue::Date(s.clone())))
                    } else {
                        from_variable(TimerKind::Date, s)
                    }
                }
                TimerSpec::Cycle(_) | TimerSpec::Missing => Err(CompileError::Internal(format!(
                    "timer '{}' with a cycle/missing definition survived lint",
                    node.id
                ))),
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
                NodeKind::Boundary(b) => match &b.trigger {
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
                        boundary_hosts.push((ix, b.attached_to.clone().unwrap_or_default()));
                        ExecKind::ErrorBoundary { code }
                    }
                    BoundaryTrigger::Timer(spec) => {
                        boundary_hosts.push((ix, b.attached_to.clone().unwrap_or_default()));
                        ExecKind::TimerBoundary {
                            due: timer_due(node, spec)?,
                        }
                    }
                    _ => {
                        return Err(CompileError::Internal(format!(
                            "unsupported boundary trigger on '{}' survived lint",
                            node.id
                        )));
                    }
                },
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
        let mut timer_boundaries: BTreeMap<NodeIx, Vec<NodeIx>> = BTreeMap::new();
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
                ExecKind::TimerBoundary { .. } => {
                    // Timer boundaries arm on any waiting host token: tasks
                    // (work items), receive tasks (subscriptions), and
                    // subprocesses (the whole scope).
                    if !matches!(
                        nodes[host].kind,
                        ExecKind::Task { .. }
                            | ExecKind::MessageCatch { .. }
                            | ExecKind::SubProcess { .. }
                    ) {
                        return Err(CompileError::Internal(format!(
                            "timer boundary '{}' on unsupported host survived lint",
                            nodes[boundary_ix].id
                        )));
                    }
                    timer_boundaries.entry(host).or_default().push(boundary_ix);
                }
                _ => unreachable!("boundary_hosts only collects boundary nodes"),
            }
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
            timer_boundaries,
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

    /// The interrupting timer boundaries armed whenever `host` starts
    /// waiting (declaration order).
    pub fn timer_boundaries(&self, host: NodeIx) -> &[NodeIx] {
        self.timer_boundaries
            .get(&host)
            .map(Vec::as_slice)
            .unwrap_or(&[])
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
