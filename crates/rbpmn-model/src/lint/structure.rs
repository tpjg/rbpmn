//! Flow-graph construction and structural well-formedness rules.
//!
//! Everything downstream (conditions, boundary checks, region analysis)
//! assumes the graph invariants established here: resolvable flows, sane
//! in/out cardinalities, connectivity, unique ids. The region analysis is
//! only run when a scope passes with zero errors.

use crate::diagnostics::{Diagnostic, rule};
use crate::model::*;
use std::collections::HashMap;

pub struct Graph<'a> {
    pub scope: &'a FlowScope,
    pub idx: HashMap<&'a str, usize>,
    pub has_duplicate_ids: bool,
    /// Per flow index: resolved (source, target) node indices, if both resolve.
    pub endpoints: Vec<Option<(usize, usize)>>,
    /// Per node index: incoming/outgoing flow indices (resolved flows only).
    pub flow_in: Vec<Vec<usize>>,
    pub flow_out: Vec<Vec<usize>>,
    /// Boundary event -> host activity, and host -> its boundary events.
    /// Modeled as pseudo-edges (host -> boundary) for reachability and the
    /// region analysis: a boundary path is part of its host's branch.
    pub host_of: Vec<Option<usize>>,
    pub boundaries: Vec<Vec<usize>>,
}

impl<'a> Graph<'a> {
    pub fn build(scope: &'a FlowScope) -> Self {
        let n = scope.nodes.len();
        let mut idx = HashMap::with_capacity(n);
        let mut has_duplicate_ids = false;
        for (i, node) in scope.nodes.iter().enumerate() {
            if idx.insert(node.id.as_str(), i).is_some() {
                has_duplicate_ids = true;
            }
        }

        let mut endpoints = Vec::with_capacity(scope.flows.len());
        let mut flow_in = vec![Vec::new(); n];
        let mut flow_out = vec![Vec::new(); n];
        for (fi, f) in scope.flows.iter().enumerate() {
            let resolved = match (idx.get(f.source.as_str()), idx.get(f.target.as_str())) {
                (Some(&s), Some(&t)) => {
                    flow_out[s].push(fi);
                    flow_in[t].push(fi);
                    Some((s, t))
                }
                _ => None,
            };
            endpoints.push(resolved);
        }

        let mut host_of = vec![None; n];
        let mut boundaries = vec![Vec::new(); n];
        for (i, node) in scope.nodes.iter().enumerate() {
            if let NodeKind::Boundary(b) = &node.kind {
                if let Some(host) = b.attached_to.as_deref().and_then(|h| idx.get(h)) {
                    host_of[i] = Some(*host);
                    boundaries[*host].push(i);
                }
            }
        }

        Graph {
            scope,
            idx,
            has_duplicate_ids,
            endpoints,
            flow_in,
            flow_out,
            host_of,
            boundaries,
        }
    }

    pub fn node(&self, i: usize) -> &'a FlowNode {
        &self.scope.nodes[i]
    }

    pub fn flow(&self, fi: usize) -> &'a SequenceFlow {
        &self.scope.flows[fi]
    }

    pub fn src(&self, fi: usize) -> usize {
        self.endpoints[fi].unwrap().0
    }

    pub fn tgt(&self, fi: usize) -> usize {
        self.endpoints[fi].unwrap().1
    }

    pub fn in_deg(&self, v: usize) -> usize {
        self.flow_in[v].len()
    }

    pub fn out_deg(&self, v: usize) -> usize {
        self.flow_out[v].len()
    }

    /// Successors over sequence flows plus host->boundary pseudo-edges.
    pub fn succs(&self, v: usize) -> Vec<usize> {
        let mut out: Vec<usize> = self.flow_out[v].iter().map(|&fi| self.tgt(fi)).collect();
        out.extend(&self.boundaries[v]);
        out
    }

    /// Predecessors over sequence flows plus the boundary's host pseudo-edge.
    pub fn preds(&self, v: usize) -> Vec<usize> {
        let mut out: Vec<usize> = self.flow_in[v].iter().map(|&fi| self.src(fi)).collect();
        if let Some(h) = self.host_of[v] {
            out.push(h);
        }
        out
    }
}

pub fn check(g: &Graph, owner: &str, out: &mut Vec<Diagnostic>) {
    // Unresolvable flows: dangling refs or flows crossing scope boundaries.
    for (fi, resolved) in g.endpoints.iter().enumerate() {
        if resolved.is_none() {
            out.push(Diagnostic::error(
                rule::BPMN_STRUCTURE,
                &g.flow(fi).id,
                "sequence flow endpoints must resolve within the same scope \
                 (flows cannot cross a subprocess boundary or reference missing elements)",
            ));
        }
    }

    let starts: Vec<usize> = (0..g.scope.nodes.len())
        .filter(|&i| matches!(g.node(i).kind, NodeKind::Start(_)))
        .collect();
    let ends: Vec<usize> = (0..g.scope.nodes.len())
        .filter(|&i| matches!(g.node(i).kind, NodeKind::End(_)))
        .collect();

    if starts.len() != 1 {
        out.push(Diagnostic::error(
            rule::SINGLE_START_EVENT,
            owner,
            format!(
                "every process and subprocess must have exactly one start event, found {}",
                starts.len()
            ),
        ));
    }

    for i in 0..g.scope.nodes.len() {
        let node = g.node(i);
        let (ins, outs) = (g.in_deg(i), g.out_deg(i));
        match &node.kind {
            // Unsupported elements are already fatal; their flow semantics are
            // unknown, so cardinality checks would only add noise.
            NodeKind::Unsupported { .. } => continue,
            NodeKind::Start(_) => {
                if ins > 0 {
                    out.push(Diagnostic::error(
                        rule::BPMN_STRUCTURE,
                        &node.id,
                        "start events cannot have incoming sequence flows",
                    ));
                }
                if outs == 0 {
                    out.push(Diagnostic::error(
                        rule::BPMN_STRUCTURE,
                        &node.id,
                        "start event needs an outgoing sequence flow",
                    ));
                } else if outs > 1 {
                    out.push(implicit_split(node));
                }
            }
            NodeKind::End(_) => {
                if outs > 0 {
                    out.push(Diagnostic::error(
                        rule::BPMN_STRUCTURE,
                        &node.id,
                        "end events cannot have outgoing sequence flows",
                    ));
                }
                if ins == 0 {
                    out.push(Diagnostic::error(
                        rule::BPMN_STRUCTURE,
                        &node.id,
                        "end event has no incoming sequence flow",
                    ));
                }
            }
            NodeKind::Boundary(_) => {
                if ins > 0 {
                    out.push(Diagnostic::error(
                        rule::BPMN_STRUCTURE,
                        &node.id,
                        "boundary events cannot have incoming sequence flows",
                    ));
                }
                if outs == 0 {
                    out.push(Diagnostic::error(
                        rule::BPMN_STRUCTURE,
                        &node.id,
                        "boundary event needs an outgoing sequence flow",
                    ));
                } else if outs > 1 {
                    out.push(implicit_split(node));
                }
            }
            kind if kind.is_gateway() => {
                if ins == 0 {
                    out.push(Diagnostic::error(
                        rule::BPMN_STRUCTURE,
                        &node.id,
                        "gateway has no incoming sequence flow",
                    ));
                }
                if outs == 0 {
                    out.push(Diagnostic::error(
                        rule::BPMN_STRUCTURE,
                        &node.id,
                        "gateway has no outgoing sequence flow",
                    ));
                }
                if ins > 1 && outs > 1 {
                    out.push(Diagnostic::error(
                        rule::NO_MIXED_GATEWAY,
                        &node.id,
                        "a gateway must either split or join, not both — \
                         use two gateways (join first, then split)",
                    ));
                }
            }
            _ => {
                if ins == 0 {
                    out.push(Diagnostic::error(
                        rule::BPMN_STRUCTURE,
                        &node.id,
                        format!("{} has no incoming sequence flow", node.kind.describe()),
                    ));
                }
                if outs == 0 {
                    out.push(Diagnostic::error(
                        rule::BPMN_STRUCTURE,
                        &node.id,
                        format!(
                            "{} is a dead end: it needs an outgoing sequence flow",
                            node.kind.describe()
                        ),
                    ));
                } else if outs > 1 {
                    out.push(implicit_split(node));
                }
            }
        }
    }

    // Connectivity. Skipped when there is no start (mass noise) — the
    // single-start-event error already tells the modeler what to fix.
    if !starts.is_empty() {
        let mut reachable = vec![false; g.scope.nodes.len()];
        let mut queue: Vec<usize> = starts.clone();
        for &s in &starts {
            reachable[s] = true;
        }
        while let Some(v) = queue.pop() {
            for s in g.succs(v) {
                if !reachable[s] {
                    reachable[s] = true;
                    queue.push(s);
                }
            }
        }
        for i in 0..g.scope.nodes.len() {
            // Nodes with zero incoming flows already got a cardinality error.
            if !reachable[i]
                && g.in_deg(i) > 0
                && !matches!(g.node(i).kind, NodeKind::Unsupported { .. })
            {
                out.push(Diagnostic::error(
                    rule::BPMN_STRUCTURE,
                    &g.node(i).id,
                    "unreachable from the start event",
                ));
            }
        }
    }

    if ends.is_empty() {
        out.push(Diagnostic::error(
            rule::BPMN_STRUCTURE,
            owner,
            "scope has no end event",
        ));
    } else {
        let mut reaches_end = vec![false; g.scope.nodes.len()];
        let mut queue: Vec<usize> = ends.clone();
        for &e in &ends {
            reaches_end[e] = true;
        }
        while let Some(v) = queue.pop() {
            for p in g.preds(v) {
                if !reaches_end[p] {
                    reaches_end[p] = true;
                    queue.push(p);
                }
            }
        }
        for i in 0..g.scope.nodes.len() {
            // Dead ends (zero outgoing) already got a cardinality error.
            if !reaches_end[i]
                && g.out_deg(i) > 0
                && !matches!(g.node(i).kind, NodeKind::Unsupported { .. })
            {
                out.push(Diagnostic::error(
                    rule::BPMN_STRUCTURE,
                    &g.node(i).id,
                    "no path to an end event (tokens would be trapped forever)",
                ));
            }
        }
    }
}

fn implicit_split(node: &FlowNode) -> Diagnostic {
    Diagnostic::error(
        rule::NO_IMPLICIT_SPLIT,
        &node.id,
        format!(
            "{} has multiple outgoing sequence flows — the spec gives this implicit \
             (and surprising) parallel/inclusive split semantics; use an explicit \
             parallel or exclusive gateway instead",
            node.kind.describe()
        ),
    )
}
