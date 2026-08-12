//! A generator for block-structured BPMN, and a structural oracle over the
//! same tree (docs/stress-testing.md §3).
//!
//! The generator is an *independent second implementation* of "what block
//! structure means" — the same thing `balanced-gateways` claims to enforce.
//! Every model it emits must lint clean (§3a), and a small interpreter over
//! the block tree predicts exactly which tasks execute and how often, without
//! running the engine (§3b). Two implementations of BPMN semantics,
//! differentially tested.
//!
//! Structural note, learned from the accepted fixtures: every block flows
//! *through* to the single process end event. Blocks never contain end events
//! of their own, which is what keeps `end-event-in-branch` out of the
//! generated corpus.

// Shared by several test binaries (`generator.rs`, `explore.rs`), each of
// which uses a different part of it.
#![allow(dead_code)]

use rbpmn_core::*;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

// ------------------------------------------------------------------ grammar

/// The shape of a model, before ids are assigned.
#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Task,
    Seq(Vec<Block>),
    /// Exclusive split/join. The last branch is the default flow.
    Xor(Vec<Block>),
    /// Parallel split/join.
    Par(Vec<Block>),
    /// A loop wrapping a whole block: exclusive join, body, control task,
    /// exclusive split with the back-edge.
    Loop(Box<Block>),
}

/// The same tree with element ids and decision variables assigned. The XML
/// emitter and the oracle both walk *this*, so they cannot disagree about
/// which element is which.
#[derive(Debug, Clone)]
pub enum Node {
    Task(String),
    Seq(Vec<Node>),
    Xor {
        /// Decision variable; branch `i` carries the condition `var = i`.
        var: String,
        branches: Vec<Node>,
    },
    Par(Vec<Node>),
    Loop {
        /// Back-edge condition variable, written by the control task.
        var: String,
        /// The control task closing the body — how the driver bounds the loop.
        ctl: String,
        body: Box<Node>,
    },
}

#[derive(Default)]
struct Ids {
    task: usize,
    xor: usize,
    loops: usize,
}

fn number(block: &Block, ids: &mut Ids) -> Node {
    match block {
        Block::Task => {
            ids.task += 1;
            Node::Task(format!("t{}", ids.task))
        }
        Block::Seq(parts) => Node::Seq(parts.iter().map(|b| number(b, ids)).collect()),
        Block::Xor(branches) => {
            ids.xor += 1;
            let var = format!("x{}", ids.xor);
            Node::Xor {
                var,
                branches: branches.iter().map(|b| number(b, ids)).collect(),
            }
        }
        Block::Par(branches) => Node::Par(branches.iter().map(|b| number(b, ids)).collect()),
        Block::Loop(body) => {
            ids.loops += 1;
            let n = ids.loops;
            ids.task += 1;
            Node::Loop {
                var: format!("l{n}"),
                ctl: format!("lctl{}", ids.task),
                body: Box::new(number(body, ids)),
            }
        }
    }
}

// ------------------------------------------------------------------ emitting

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Start,
    End,
    UserTask,
    Exclusive,
    Parallel,
}

struct Element {
    id: String,
    kind: Kind,
}

struct Flow {
    id: String,
    source: String,
    target: String,
    condition: Option<String>,
}

/// Elements and flows are collected separately and stitched at the end: an
/// element's `<incoming>`/`<outgoing>` lists are *derived* from the flows, so
/// nothing has to know its successors while being emitted.
#[derive(Default)]
struct Builder {
    elements: Vec<Element>,
    flows: Vec<Flow>,
}

impl Builder {
    fn element(&mut self, id: &str, kind: Kind) {
        self.elements.push(Element {
            id: id.to_string(),
            kind,
        });
    }

    fn flow(&mut self, source: &str, target: &str, condition: Option<String>) {
        let id = format!("f{}", self.flows.len() + 1);
        self.flows.push(Flow {
            id,
            source: source.to_string(),
            target: target.to_string(),
            condition,
        });
    }

    /// Add `node`'s elements, entered from element `from` by a new flow
    /// carrying `cond`. Returns the block's exit element.
    fn add(&mut self, node: &Node, from: &str, cond: Option<String>) -> String {
        match node {
            Node::Task(id) => {
                self.element(id, Kind::UserTask);
                self.flow(from, id, cond);
                id.clone()
            }
            Node::Seq(parts) => {
                let mut current = from.to_string();
                let mut pending = cond;
                for part in parts {
                    current = self.add(part, &current, pending.take());
                }
                current
            }
            Node::Xor { var, branches } => {
                let n = self.elements.len();
                let (split, join) = (format!("xs{n}"), format!("xj{n}"));
                self.element(&split, Kind::Exclusive);
                self.flow(from, &split, cond);
                let mut exits = Vec::new();
                for (i, branch) in branches.iter().enumerate() {
                    // Every branch but the last is conditional; the last is
                    // the default flow (see `default_flow_of` on emit).
                    let c = (i + 1 < branches.len()).then(|| format!("{var} = {i}"));
                    exits.push(self.add(branch, &split, c));
                }
                self.element(&join, Kind::Exclusive);
                for exit in exits {
                    self.flow(&exit, &join, None);
                }
                join
            }
            Node::Par(branches) => {
                let n = self.elements.len();
                let (split, join) = (format!("ps{n}"), format!("pj{n}"));
                self.element(&split, Kind::Parallel);
                self.flow(from, &split, cond);
                let exits: Vec<String> =
                    branches.iter().map(|b| self.add(b, &split, None)).collect();
                self.element(&join, Kind::Parallel);
                for exit in exits {
                    self.flow(&exit, &join, None);
                }
                join
            }
            Node::Loop { var, ctl, body } => {
                let n = self.elements.len();
                let (entry, exit) = (format!("lj{n}"), format!("ls{n}"));
                self.element(&entry, Kind::Exclusive);
                self.flow(from, &entry, cond);
                let body_exit = self.add(body, &entry, None);
                self.element(ctl, Kind::UserTask);
                self.flow(&body_exit, ctl, None);
                self.element(&exit, Kind::Exclusive);
                self.flow(ctl, &exit, None);
                // Back-edge first, so the loop's *exit* flow is the last
                // outgoing and therefore becomes the default.
                self.flow(&exit, &entry, Some(format!("{var} = true")));
                exit
            }
        }
    }

    fn incoming(&self, id: &str) -> Vec<&str> {
        self.flows
            .iter()
            .filter(|f| f.target == id)
            .map(|f| f.id.as_str())
            .collect()
    }

    fn outgoing(&self, id: &str) -> Vec<&str> {
        self.flows
            .iter()
            .filter(|f| f.source == id)
            .map(|f| f.id.as_str())
            .collect()
    }

    fn to_xml(&self) -> String {
        let mut body = String::new();
        for e in &self.elements {
            let inc: String = self
                .incoming(&e.id)
                .iter()
                .map(|f| format!("<bpmn:incoming>{f}</bpmn:incoming>"))
                .collect();
            let out: String = self
                .outgoing(&e.id)
                .iter()
                .map(|f| format!("<bpmn:outgoing>{f}</bpmn:outgoing>"))
                .collect();
            let (tag, attrs) = match e.kind {
                Kind::Start => ("bpmn:startEvent", String::new()),
                Kind::End => ("bpmn:endEvent", String::new()),
                Kind::UserTask => ("bpmn:userTask", String::new()),
                Kind::Parallel => ("bpmn:parallelGateway", String::new()),
                Kind::Exclusive => {
                    // An exclusive split needs a default flow; by construction
                    // it is always the last outgoing one.
                    let outs = self.outgoing(&e.id);
                    match outs.len() {
                        0 | 1 => ("bpmn:exclusiveGateway", String::new()),
                        _ => (
                            "bpmn:exclusiveGateway",
                            format!(" default=\"{}\"", outs[outs.len() - 1]),
                        ),
                    }
                }
            };
            body.push_str(&format!(
                "    <{tag} id=\"{}\"{attrs}>{inc}{out}</{tag}>\n",
                e.id
            ));
        }
        for f in &self.flows {
            match &f.condition {
                None => body.push_str(&format!(
                    "    <bpmn:sequenceFlow id=\"{}\" sourceRef=\"{}\" targetRef=\"{}\" />\n",
                    f.id, f.source, f.target
                )),
                Some(c) => body.push_str(&format!(
                    "    <bpmn:sequenceFlow id=\"{}\" sourceRef=\"{}\" targetRef=\"{}\">\
                     <bpmn:conditionExpression>{c}</bpmn:conditionExpression>\
                     </bpmn:sequenceFlow>\n",
                    f.id, f.source, f.target
                )),
            }
        }
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <bpmn:definitions xmlns:bpmn=\"http://www.omg.org/spec/BPMN/20100524/MODEL\" \
             id=\"defs\" targetNamespace=\"https://rbpmn.dev/generated\">\n\
             \x20 <bpmn:process id=\"p\" isExecutable=\"true\">\n{body}  </bpmn:process>\n\
             </bpmn:definitions>\n"
        )
    }
}

pub struct Generated {
    pub xml: String,
    pub root: Node,
}

pub fn build(block: &Block) -> Generated {
    let root = number(block, &mut Ids::default());
    let mut b = Builder::default();
    b.element("start", Kind::Start);
    let exit = b.add(&root, "start", None);
    b.element("end", Kind::End);
    b.flow(&exit, "end", None);
    Generated {
        xml: b.to_xml(),
        root,
    }
}

// ---------------------------------------------------------------- decisions

/// Which branch each exclusive split takes, and how many times each loop runs.
/// Shared input to the oracle and the driver — that is what makes the
/// comparison meaningful.
#[derive(Debug, Clone, Default)]
pub struct Decisions {
    pub xor: BTreeMap<String, usize>,
    pub loops: BTreeMap<String, usize>,
}

/// Deterministic, tiny, and self-contained — the model is what proptest
/// shrinks; decisions only need to be reproducible.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(2) | 1)
    }
    pub fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next() as usize) % n
        }
    }
}

pub fn decide(root: &Node, rng: &mut Rng, max_iterations: usize) -> Decisions {
    fn walk(node: &Node, rng: &mut Rng, max: usize, out: &mut Decisions) {
        match node {
            Node::Task(_) => {}
            Node::Seq(parts) | Node::Par(parts) => {
                parts.iter().for_each(|p| walk(p, rng, max, out))
            }
            Node::Xor { var, branches } => {
                out.xor.insert(var.clone(), rng.below(branches.len()));
                branches.iter().for_each(|b| walk(b, rng, max, out));
            }
            Node::Loop { var, body, .. } => {
                out.loops.insert(var.clone(), 1 + rng.below(max));
                walk(body, rng, max, out);
            }
        }
    }
    let mut out = Decisions::default();
    walk(root, rng, max_iterations, &mut out);
    out
}

/// The initial variable document: every exclusive split's choice, decided up
/// front. Loop variables are written by the control tasks instead.
pub fn initial_variables(dec: &Decisions) -> Value {
    let mut map = Map::new();
    for (var, choice) in &dec.xor {
        map.insert(var.clone(), json!(choice));
    }
    Value::Object(map)
}

// ------------------------------------------------------------------- oracle

/// **The oracle.** How many times each task must execute, derived from the
/// block tree alone — no engine involved. This is the second implementation
/// the differential compares against.
pub fn expected_executions(root: &Node, dec: &Decisions) -> BTreeMap<String, usize> {
    fn walk(node: &Node, dec: &Decisions, out: &mut BTreeMap<String, usize>) {
        match node {
            Node::Task(id) => *out.entry(id.clone()).or_default() += 1,
            Node::Seq(parts) | Node::Par(parts) => parts.iter().for_each(|p| walk(p, dec, out)),
            Node::Xor { var, branches } => {
                let choice = dec.xor.get(var).copied().unwrap_or(branches.len() - 1);
                walk(&branches[choice], dec, out);
            }
            Node::Loop { var, ctl, body } => {
                for _ in 0..dec.loops.get(var).copied().unwrap_or(1) {
                    walk(body, dec, out);
                    *out.entry(ctl.clone()).or_default() += 1;
                }
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, dec, &mut out);
    out
}

// ------------------------------------------------------------------- driver

/// Control tasks, mapped to the loop variable they close.
fn control_tasks(root: &Node) -> BTreeMap<String, String> {
    fn walk(node: &Node, out: &mut BTreeMap<String, String>) {
        match node {
            Node::Task(_) => {}
            Node::Seq(parts) | Node::Par(parts) => parts.iter().for_each(|p| walk(p, out)),
            Node::Xor { branches, .. } => branches.iter().for_each(|b| walk(b, out)),
            Node::Loop { var, ctl, body } => {
                out.insert(ctl.clone(), var.clone());
                walk(body, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, &mut out);
    out
}

pub struct Run {
    pub executions: BTreeMap<String, usize>,
    pub status: InstanceStatus,
    pub steps: usize,
}

/// Drive the engine to completion under `dec`, completing whichever open work
/// item `rng` picks — so interleaving varies while the outcome must not.
/// Returns how many times the *engine* actually ran each task.
pub fn run(
    proc: &ExecutableProcess,
    root: &Node,
    dec: &Decisions,
    rng: &mut Rng,
    step_budget: usize,
) -> Result<Run, String> {
    let controls = control_tasks(root);
    // Iterations still owed per loop, refilled when the loop exits — which is
    // what makes nested loops come out right on re-entry.
    let mut remaining: BTreeMap<String, usize> = controls
        .iter()
        .map(|(ctl, var)| (ctl.clone(), dec.loops.get(var).copied().unwrap_or(1)))
        .collect();

    let mut state = InstanceState::new();
    step(
        proc,
        &mut state,
        Command::Start {
            variables: initial_variables(dec),
        },
    )
    .map_err(|e| format!("start: {e}"))?;

    let mut executions: BTreeMap<String, usize> = BTreeMap::new();
    let mut steps = 0usize;
    while state.status == InstanceStatus::Active {
        steps += 1;
        if steps > step_budget {
            return Err(format!(
                "step budget {step_budget} exhausted — likely a loop that never exits"
            ));
        }
        let open: Vec<(WorkItemId, String)> = state
            .open_work_items()
            .map(|(id, w)| (id, proc.node_id(w.element).to_string()))
            .collect();
        if open.is_empty() {
            return Err(format!(
                "active instance with no open work item after {steps} steps"
            ));
        }
        let (id, element) = open[rng.below(open.len())].clone();

        let patch = match controls.get(&element) {
            None => json!({}),
            Some(var) => {
                let left = remaining.get_mut(&element).expect("control task budget");
                *left -= 1;
                if *left == 0 {
                    // Last pass: leave the loop, and refill for a possible
                    // re-entry from an enclosing loop.
                    *left = dec.loops.get(var).copied().unwrap_or(1);
                    json!({ var.clone(): false })
                } else {
                    json!({ var.clone(): true })
                }
            }
        };
        *executions.entry(element.clone()).or_default() += 1;
        step(proc, &mut state, Command::CompleteWorkItem { id, patch })
            .map_err(|e| format!("completing {element}: {e}"))?;
    }

    Ok(Run {
        executions,
        status: state.status,
        steps,
    })
}
