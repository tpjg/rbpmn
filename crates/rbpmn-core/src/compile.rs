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

/// The per-definition wiring from the deployment manifest that the core needs
/// today: element -> work-item topic. Unmapped tasks default to their element
/// id. (Correlations join in phase 3.)
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Bindings {
    #[serde(default)]
    pub topics: BTreeMap<String, String>,
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
    Task { kind: WorkKind, topic: String },
    ExclusiveGateway { default_flow: Option<FlowIx> },
    ParallelGateway,
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

        let mut nodes = Vec::with_capacity(scope.nodes.len());
        let mut node_ix: BTreeMap<&str, NodeIx> = BTreeMap::new();
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
                NodeKind::Start(StartTrigger::Message(_)) | NodeKind::End(EndKind::Message(_)) => {
                    return Err(not_yet(node, "messages arrive in phase 3"));
                }
                NodeKind::Catch(_) | NodeKind::Throw(_) | NodeKind::EventBasedGateway => {
                    return Err(not_yet(node, "messages and timers arrive in phase 3"));
                }
                NodeKind::ReceiveTask { .. } => {
                    return Err(not_yet(node, "messages arrive in phase 3"));
                }
                NodeKind::Boundary(_) => {
                    return Err(not_yet(
                        node,
                        "error boundaries arrive in phase 2, timer boundaries in phase 3",
                    ));
                }
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
