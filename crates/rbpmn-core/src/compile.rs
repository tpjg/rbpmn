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
        due: TimerDue,
    },
    /// Message catch — an `intermediateCatchEvent` or a `receiveTask` (same
    /// semantics): parks its token behind a subscription. `key` is the
    /// parsed correlation qualified name, `key_name` its source text.
    MessageCatch {
        message: String,
        key: Vec<String>,
        key_name: String,
    },
    /// Interrupting timer boundary: armed on the host's token, entered only
    /// by its timer firing — never via a sequence flow.
    TimerBoundary {
        due: TimerDue,
    },
    /// Parks its token and arms every target catch event; the first to fire
    /// wins and the rest are cancelled.
    EventBasedGateway,
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
        let mut correlation = |node: &FlowNode| -> Result<(Vec<String>, String), CompileError> {
            let Some(name) = bindings.correlations.get(&node.id) else {
                missing_correlations.push(node.id.clone());
                return Ok((Vec::new(), String::new())); // placeholder; rejected below
            };
            let path =
                condition::parse_qname(name).map_err(|e| CompileError::InvalidCorrelation {
                    element: node.id.clone(),
                    reason: e.to_string(),
                })?;
            Ok((path, name.clone()))
        };
        let timer_due = |node: &FlowNode, spec: &TimerSpec| -> Result<TimerDue, CompileError> {
            match spec {
                TimerSpec::Duration(s) => Ok(TimerDue::Duration(s.clone())),
                TimerSpec::Date(s) => Ok(TimerDue::Date(s.clone())),
                TimerSpec::Cycle(_) | TimerSpec::Missing => Err(CompileError::Internal(format!(
                    "timer '{}' with a cycle/missing definition survived lint",
                    node.id
                ))),
            }
        };

        let mut nodes = Vec::with_capacity(scope.nodes.len());
        let mut node_ix: BTreeMap<&str, NodeIx> = BTreeMap::new();
        let mut boundary_hosts: Vec<(NodeIx, String)> = Vec::new();
        for (ix, node) in scope.nodes.iter().enumerate() {
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
                NodeKind::UserTask => ExecKind::Task {
                    kind: WorkKind::User,
                    topic: bindings
                        .topics
                        .get(&node.id)
                        .cloned()
                        .unwrap_or_else(|| node.id.clone()),
                },
                NodeKind::ExclusiveGateway { .. } => {
                    // The default flow index is resolved after flows are built.
                    ExecKind::ExclusiveGateway { default_flow: None }
                }
                NodeKind::ParallelGateway => ExecKind::ParallelGateway,
                NodeKind::EventBasedGateway => ExecKind::EventBasedGateway,
                NodeKind::Catch(CatchTrigger::Timer(spec)) => ExecKind::TimerCatch {
                    due: timer_due(node, spec)?,
                },
                NodeKind::Catch(CatchTrigger::Message(message_ref)) => {
                    let (key, key_name) = correlation(node)?;
                    ExecKind::MessageCatch {
                        message: message_name(node, message_ref)?,
                        key,
                        key_name,
                    }
                }
                NodeKind::ReceiveTask { message_ref } => {
                    // A receive task IS a message catch (identical semantics,
                    // task-shaped notation) — it never creates a work item.
                    let (key, key_name) = correlation(node)?;
                    ExecKind::MessageCatch {
                        message: message_name(node, message_ref)?,
                        key,
                        key_name,
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
                NodeKind::SubProcess(_) => {
                    return Err(not_yet(node, "embedded subprocesses arrive in v2"));
                }
                other => {
                    return Err(CompileError::Internal(format!(
                        "unexpected {} '{}' survived lint",
                        other.describe(),
                        node.id
                    )));
                }
            };
            node_ix.insert(node.id.as_str(), ix);
            nodes.push(ExecNode {
                id: node.id.clone(),
                kind,
                incoming: Vec::new(),
                outgoing: Vec::new(),
            });
        }
        if !missing_correlations.is_empty() {
            return Err(CompileError::MissingCorrelation(missing_correlations));
        }

        let mut flows = Vec::with_capacity(scope.flows.len());
        for (fi, flow) in scope.flows.iter().enumerate() {
            let source = *node_ix
                .get(flow.source.as_str())
                .ok_or_else(|| CompileError::Internal(format!("dangling flow '{}'", flow.id)))?;
            let target = *node_ix
                .get(flow.target.as_str())
                .ok_or_else(|| CompileError::Internal(format!("dangling flow '{}'", flow.id)))?;
            let cond = flow
                .condition
                .as_deref()
                .map(condition::parse)
                .transpose()
                .map_err(|e| CompileError::Internal(format!("condition on '{}': {e}", flow.id)))?;
            nodes[source].outgoing.push(fi);
            nodes[target].incoming.push(fi);
            flows.push(ExecFlow {
                id: flow.id.clone(),
                source,
                target,
                condition: cond,
            });
        }

        for node in &mut nodes {
            if let ExecKind::ExclusiveGateway { default_flow } = &mut node.kind {
                let model_node = scope.nodes.iter().find(|n| n.id == node.id).unwrap();
                if let NodeKind::ExclusiveGateway {
                    default_flow: Some(id),
                } = &model_node.kind
                {
                    *default_flow = flows.iter().position(|f| &f.id == id);
                }
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
                    // v1: errors originate from failing service tasks; a
                    // boundary on anything else (subprocesses are v2) is not
                    // executable yet.
                    if !matches!(
                        nodes[host].kind,
                        ExecKind::Task {
                            kind: WorkKind::Service,
                            ..
                        }
                    ) {
                        return Err(CompileError::NotYetExecutable {
                            element: nodes[boundary_ix].id.clone(),
                            what: "error boundary event".to_string(),
                            phase: "error boundaries on subprocesses arrive in v2",
                        });
                    }
                    error_boundaries
                        .entry(host)
                        .or_default()
                        .push((code.clone(), boundary_ix));
                }
                ExecKind::TimerBoundary { .. } => {
                    // Timer boundaries arm on any waiting host token: tasks
                    // (work items) and receive tasks (subscriptions).
                    // Subprocess hosts cannot reach here — a subprocess in
                    // the model already failed compilation above.
                    if !matches!(
                        nodes[host].kind,
                        ExecKind::Task { .. } | ExecKind::MessageCatch { .. }
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

        let start = nodes
            .iter()
            .position(|n| n.kind == ExecKind::Start)
            .ok_or_else(|| CompileError::Internal("no start event".to_string()))?;

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
