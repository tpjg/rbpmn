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
//! generated corpus. `MsgBoundary` is the production that had to obey it the
//! hard way: the accepted boundary fixtures run their handler to an end event
//! of its own, which inside a parallel branch would starve the join, so the
//! generated shape **merges back** instead (see `Block::MsgBoundary`).

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
    /// A user task carrying an interrupting message boundary, whose path runs
    /// the wrapped block and then **merges back** into the host's
    /// continuation through an exclusive gateway.
    ///
    /// Two shapes were possible and only one is generable. The accepted
    /// fixtures end a boundary path at its own end event; inside a parallel
    /// branch that is `end-event-in-branch` and would starve the join. The
    /// merge is an *uncontrolled* one in BPMN's vocabulary and legal here for
    /// the reason `implicit-merge-after-parallel` exists to check: exactly one
    /// of the two paths is ever taken, so two tokens can never arrive. It is
    /// routed through an exclusive gateway rather than straight into whatever
    /// comes next, because `balanced-gateways` counts *edges* into a parallel
    /// join and demands exactly one per branch — two would be refused even
    /// though only one can ever carry a token.
    MsgBoundary(Box<Block>),
    /// An embedded subprocess wrapping a whole block. Semantically a no-op —
    /// `Sub(B)` executes exactly what `B` does — which is what makes it a
    /// sharp oracle test: any scope bookkeeping that leaks into execution
    /// shows up as a task count that no longer matches the plain block.
    Sub(Box<Block>),
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
    /// Host task, its interrupting message boundary, and the boundary's path.
    /// The exclusive gateway the two paths merge at is named by the emitter,
    /// like every other gateway here.
    MsgBoundary {
        /// The host: a user task, so it parks on a work item the driver can
        /// complete — the other half of the race the message runs.
        task: String,
        /// The `boundaryEvent`; also the element the correlation binding is
        /// registered under (`Bindings::correlation`, never in the XML).
        boundary: String,
        /// The `bpmn:message` element id. Unique per boundary, so concurrent
        /// arms in sibling parallel branches can never collide on
        /// `(message, key)`.
        message: String,
        body: Box<Node>,
    },
    Sub {
        /// The `subProcess` element; also the scope's owner.
        id: String,
        /// The scope's own start and end events.
        start: String,
        end: String,
        body: Box<Node>,
    },
}

#[derive(Default)]
struct Ids {
    task: usize,
    xor: usize,
    loops: usize,
    subs: usize,
    boundaries: usize,
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
        Block::MsgBoundary(body) => {
            ids.boundaries += 1;
            ids.task += 1;
            let n = ids.boundaries;
            Node::MsgBoundary {
                task: format!("t{}", ids.task),
                boundary: format!("b{n}"),
                message: format!("msg{n}"),
                body: Box::new(number(body, ids)),
            }
        }
        Block::Sub(body) => {
            ids.subs += 1;
            let n = ids.subs;
            Node::Sub {
                id: format!("sp{n}"),
                start: format!("sp{n}_start"),
                end: format!("sp{n}_end"),
                body: Box::new(number(body, ids)),
            }
        }
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

/// Deliberately `Copy` and payload-free: `tests/mutation.rs` swaps kinds in
/// place. What a message boundary needs beyond its kind rides on `Element`.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Start,
    End,
    UserTask,
    Exclusive,
    Parallel,
    /// Never generated — only reachable by mutation (tests/mutation.rs).
    Inclusive,
    SubProcess,
    /// Interrupting message boundary; see `Element::boundary` for its host
    /// and the message it catches.
    MessageBoundary,
}

/// The host and message of a `Kind::MessageBoundary`.
#[derive(Clone, Debug)]
pub struct BoundaryRefs {
    pub attached_to: String,
    /// The `bpmn:message` element id, not its name.
    pub message: String,
}

/// A `bpmn:message` root element. Its **name** is what `correlate()` addresses
/// and what the trace prints; the id is only what `messageRef` points at.
#[derive(Clone, Debug)]
pub struct Message {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug)]
pub struct Element {
    pub id: String,
    pub kind: Kind,
    /// The `subProcess` element this lives inside; `None` is the process body.
    pub container: Option<String>,
    /// Set on `Kind::MessageBoundary` only.
    pub boundary: Option<BoundaryRefs>,
}

#[derive(Clone, Debug)]
pub struct Flow {
    pub id: String,
    pub source: String,
    pub target: String,
    pub condition: Option<String>,
    /// Flows are declared inside the scope they belong to; a flow that ends
    /// up crossing scopes is exactly what `cross-scope-flow` rejects.
    pub container: Option<String>,
}

/// Elements and flows are collected separately and stitched at the end: an
/// element's `<incoming>`/`<outgoing>` lists are *derived* from the flows, so
/// nothing has to know its successors while being emitted.
#[derive(Default, Clone, Debug)]
pub struct Builder {
    pub elements: Vec<Element>,
    pub flows: Vec<Flow>,
    /// `bpmn:message` root elements — one per message boundary, so no two
    /// concurrent arms can ever share a `(message, key)` pair.
    pub messages: Vec<Message>,
    /// Scope currently being emitted into.
    container: Option<String>,
}

impl Builder {
    fn element(&mut self, id: &str, kind: Kind) {
        self.elements.push(Element {
            id: id.to_string(),
            kind,
            container: self.container.clone(),
            boundary: None,
        });
    }

    /// A boundary event lives in its host's container and carries no incoming
    /// flow: it is reached through the attachment, not through the graph.
    fn boundary_element(&mut self, id: &str, attached_to: &str, message: &str) {
        self.elements.push(Element {
            id: id.to_string(),
            kind: Kind::MessageBoundary,
            container: self.container.clone(),
            boundary: Some(BoundaryRefs {
                attached_to: attached_to.to_string(),
                message: message.to_string(),
            }),
        });
    }

    fn flow(&mut self, source: &str, target: &str, condition: Option<String>) {
        let id = format!("f{}", self.flows.len() + 1);
        self.flows.push(Flow {
            id,
            source: source.to_string(),
            target: target.to_string(),
            condition,
            container: self.container.clone(),
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
            Node::MsgBoundary {
                task,
                boundary,
                message,
                body,
            } => {
                self.element(task, Kind::UserTask);
                self.flow(from, task, cond);
                self.messages.push(Message {
                    id: message.clone(),
                    name: message.to_uppercase(),
                });
                self.boundary_element(boundary, task, message);
                // The boundary's path, and then the merge. The merge gateway
                // is what keeps the block's exit a *single* element, so every
                // enclosing production — a parallel branch above all — sees
                // one edge leaving, exactly as it would from a plain task.
                let handled = self.add(body, boundary, None);
                let n = self.elements.len();
                let merge = format!("bm{n}");
                self.element(&merge, Kind::Exclusive);
                self.flow(task, &merge, None);
                self.flow(&handled, &merge, None);
                merge
            }
            Node::Sub {
                id,
                start,
                end,
                body,
            } => {
                self.element(id, Kind::SubProcess);
                self.flow(from, id, cond);
                // Everything below lives in the subprocess's own scope.
                let outer = self.container.replace(id.clone());
                self.element(start, Kind::Start);
                let body_exit = self.add(body, start, None);
                self.element(end, Kind::End);
                self.flow(&body_exit, end, None);
                self.container = outer;
                id.clone()
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

    pub fn to_xml(&self) -> String {
        let body = self.emit_container(None);
        // Messages are root elements of the definitions, beside the process —
        // the shape `accept/29-message-boundary.bpmn` uses.
        let messages: String = self
            .messages
            .iter()
            .map(|m| format!("  <bpmn:message id=\"{}\" name=\"{}\" />\n", m.id, m.name))
            .collect();
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <bpmn:definitions xmlns:bpmn=\"http://www.omg.org/spec/BPMN/20100524/MODEL\" \
             id=\"defs\" targetNamespace=\"https://rbpmn.dev/generated\">\n{messages}\
             \x20 <bpmn:process id=\"p\" isExecutable=\"true\">\n{body}  </bpmn:process>\n\
             </bpmn:definitions>\n"
        )
    }

    /// Emit one scope. A subprocess's children are nested *inside* its
    /// element, which is what makes the emitted document a real scope tree
    /// rather than a flat graph with a label on it.
    fn emit_container(&self, container: Option<&str>) -> String {
        let mut out = String::new();
        for e in self
            .elements
            .iter()
            .filter(|e| e.container.as_deref() == container)
        {
            let inc: String = self
                .incoming(&e.id)
                .iter()
                .map(|f| format!("<bpmn:incoming>{f}</bpmn:incoming>"))
                .collect();
            let outs_list = self.outgoing(&e.id);
            let out_tags: String = outs_list
                .iter()
                .map(|f| format!("<bpmn:outgoing>{f}</bpmn:outgoing>"))
                .collect();
            if e.kind == Kind::SubProcess {
                out.push_str(&format!(
                    "    <bpmn:subProcess id=\"{}\">{inc}{out_tags}\n{}    </bpmn:subProcess>\n",
                    e.id,
                    self.emit_container(Some(&e.id))
                ));
                continue;
            }
            if e.kind == Kind::MessageBoundary {
                let b = e.boundary.as_ref().expect("a boundary knows its host");
                // `{inc}` is always empty by construction; it is emitted so a
                // mutation that points a flow *at* a boundary produces the
                // invalid document the linter should refuse, not a silently
                // dropped edge.
                out.push_str(&format!(
                    "    <bpmn:boundaryEvent id=\"{}\" attachedToRef=\"{}\">{inc}{out_tags}\
                     <bpmn:messageEventDefinition messageRef=\"{}\" />\
                     </bpmn:boundaryEvent>\n",
                    e.id, b.attached_to, b.message
                ));
                continue;
            }
            let (tag, attrs) = match e.kind {
                Kind::Start => ("bpmn:startEvent", String::new()),
                Kind::End => ("bpmn:endEvent", String::new()),
                Kind::UserTask => ("bpmn:userTask", String::new()),
                Kind::Parallel => ("bpmn:parallelGateway", String::new()),
                Kind::Inclusive => ("bpmn:inclusiveGateway", String::new()),
                Kind::SubProcess | Kind::MessageBoundary => unreachable!("handled above"),
                Kind::Exclusive => {
                    // An exclusive split needs a default flow; by construction
                    // it is always the last outgoing one.
                    match outs_list.len() {
                        0 | 1 => ("bpmn:exclusiveGateway", String::new()),
                        _ => (
                            "bpmn:exclusiveGateway",
                            format!(" default=\"{}\"", outs_list[outs_list.len() - 1]),
                        ),
                    }
                }
            };
            out.push_str(&format!(
                "    <{tag} id=\"{}\"{attrs}>{inc}{out_tags}</{tag}>\n",
                e.id
            ));
        }
        for f in self
            .flows
            .iter()
            .filter(|f| f.container.as_deref() == container)
        {
            match &f.condition {
                None => out.push_str(&format!(
                    "    <bpmn:sequenceFlow id=\"{}\" sourceRef=\"{}\" targetRef=\"{}\" />\n",
                    f.id, f.source, f.target
                )),
                Some(c) => out.push_str(&format!(
                    "    <bpmn:sequenceFlow id=\"{}\" sourceRef=\"{}\" targetRef=\"{}\">\
                     <bpmn:conditionExpression>{c}</bpmn:conditionExpression>\
                     </bpmn:sequenceFlow>\n",
                    f.id, f.source, f.target
                )),
            }
        }
        out
    }
}

pub struct Generated {
    pub xml: String,
    pub root: Node,
    /// The elements and flows behind `xml` — the surface mutations act on.
    pub skeleton: Builder,
    /// The manifest the model needs to compile: one correlation per message
    /// boundary, keyed by the boundary's own element id. Empty for a model
    /// without boundaries, so it stays `Bindings::default()` there.
    pub bindings: Bindings,
}

/// The FEEL qualified name every generated boundary correlates on. One name is
/// enough: the *message* differs per boundary, so `(message, key)` is distinct
/// even when two boundaries are armed at once in sibling parallel branches.
pub const CORRELATION_NAME: &str = "corr.key";

/// The value at `CORRELATION_NAME`. A string, because `subscribe` accepts only
/// strings and exact integers as keys.
pub const CORRELATION_VALUE: &str = "K";

pub fn build(block: &Block) -> Generated {
    let root = number(block, &mut Ids::default());
    let mut b = Builder::default();
    b.element("start", Kind::Start);
    let exit = b.add(&root, "start", None);
    b.element("end", Kind::End);
    b.flow(&exit, "end", None);
    let mut bindings = Bindings::default();
    for boundary in boundary_hosts(&root).into_values() {
        bindings = bindings.correlation(boundary, CORRELATION_NAME);
    }
    Generated {
        xml: b.to_xml(),
        root,
        skeleton: b,
        bindings,
    }
}

/// Host task id -> the message boundary armed on it. The driver's map from
/// "an open work item turned up" to "there is a message that could take it
/// away instead".
pub fn boundary_hosts(root: &Node) -> BTreeMap<String, String> {
    fn walk(node: &Node, out: &mut BTreeMap<String, String>) {
        match node {
            Node::Task(_) => {}
            Node::Seq(parts) | Node::Par(parts) => parts.iter().for_each(|p| walk(p, out)),
            Node::Xor { branches, .. } => branches.iter().for_each(|b| walk(b, out)),
            Node::Sub { body, .. } | Node::Loop { body, .. } => walk(body, out),
            Node::MsgBoundary {
                task,
                boundary,
                body,
                ..
            } => {
                out.insert(task.clone(), boundary.clone());
                walk(body, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, &mut out);
    out
}

// ---------------------------------------------------------------- decisions

/// Which branch each exclusive split takes, and how many times each loop runs.
/// Shared input to the oracle and the driver — that is what makes the
/// comparison meaningful.
#[derive(Debug, Clone, Default)]
pub struct Decisions {
    pub xor: BTreeMap<String, usize>,
    pub loops: BTreeMap<String, usize>,
    /// Per message boundary, one choice per *activation* of its host: `true`
    /// delivers the message, `false` completes the work item. A schedule
    /// rather than a single flag, so a loop around a boundary can complete on
    /// one pass and be interrupted on the next — which is where re-arming
    /// after a withdrawal actually gets tested.
    pub deliver: BTreeMap<String, Vec<bool>>,
}

impl Decisions {
    /// Does `boundary`'s `n`-th activation deliver? Past the end of the
    /// schedule the answer is "complete the host". The oracle and the driver
    /// both read the choice through *this* function, so they cannot disagree
    /// about what a missing entry means.
    pub fn delivers(&self, boundary: &str, activation: usize) -> bool {
        self.deliver
            .get(boundary)
            .and_then(|s| s.get(activation))
            .copied()
            .unwrap_or(false)
    }
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
    /// `reps` is how often the enclosing loops can activate this node — the
    /// length a boundary's schedule needs to cover every activation.
    fn walk(node: &Node, rng: &mut Rng, max: usize, reps: usize, out: &mut Decisions) {
        match node {
            Node::Task(_) => {}
            Node::Seq(parts) | Node::Par(parts) => {
                parts.iter().for_each(|p| walk(p, rng, max, reps, out))
            }
            Node::Xor { var, branches } => {
                out.xor.insert(var.clone(), rng.below(branches.len()));
                branches.iter().for_each(|b| walk(b, rng, max, reps, out));
            }
            Node::Sub { body, .. } => walk(body, rng, max, reps, out),
            Node::MsgBoundary { boundary, body, .. } => {
                let schedule = (0..reps).map(|_| rng.below(2) == 0).collect();
                out.deliver.insert(boundary.clone(), schedule);
                walk(body, rng, max, reps, out);
            }
            Node::Loop { var, body, .. } => {
                let n = 1 + rng.below(max);
                out.loops.insert(var.clone(), n);
                walk(body, rng, max, reps * n, out);
            }
        }
    }
    let mut out = Decisions::default();
    walk(root, rng, max_iterations, 1, &mut out);
    out
}

/// The initial variable document: every exclusive split's choice, decided up
/// front, plus the correlation key every message boundary resolves at arm
/// time. Loop variables are written by the control tasks instead.
///
/// The key is written unconditionally. It costs one field on a model with no
/// boundary, and it means the document is *always* the one a generated model
/// can arm against — a document that only sometimes carries the key is a
/// `correlation-failed` freeze waiting for the first caller who forgot.
pub fn initial_variables(dec: &Decisions) -> Value {
    let mut map = Map::new();
    for (var, choice) in &dec.xor {
        map.insert(var.clone(), json!(choice));
    }
    map.insert("corr".to_string(), json!({ "key": CORRELATION_VALUE }));
    Value::Object(map)
}

// ------------------------------------------------------------------- oracle

/// **The oracle.** How many times each element must execute, derived from the
/// block tree alone — no engine involved. This is the second implementation
/// the differential compares against.
///
/// One entry per unit of work the driver performs: a task counts when its work
/// item is *completed*, and a message boundary counts when its message is
/// *delivered*. A host whose message arrived therefore counts **not at all** —
/// it started, its work item was cancelled, and it never completed — while its
/// boundary and the boundary's path count instead. Never both: that is exactly
/// the property an interrupting boundary claims.
pub fn expected_executions(root: &Node, dec: &Decisions) -> BTreeMap<String, usize> {
    fn walk(
        node: &Node,
        dec: &Decisions,
        seen: &mut BTreeMap<String, usize>,
        out: &mut BTreeMap<String, usize>,
    ) {
        match node {
            Node::Task(id) => *out.entry(id.clone()).or_default() += 1,
            Node::Seq(parts) | Node::Par(parts) => {
                parts.iter().for_each(|p| walk(p, dec, seen, out))
            }
            Node::Xor { var, branches } => {
                let choice = dec.xor.get(var).copied().unwrap_or(branches.len() - 1);
                walk(&branches[choice], dec, seen, out);
            }
            // A subprocess is transparent to the oracle: entering a scope
            // executes its body, nothing more.
            Node::Sub { body, .. } => walk(body, dec, seen, out),
            Node::MsgBoundary {
                task,
                boundary,
                body,
                ..
            } => {
                let activation = seen.entry(boundary.clone()).or_default();
                let delivered = dec.delivers(boundary, *activation);
                *activation += 1;
                if delivered {
                    *out.entry(boundary.clone()).or_default() += 1;
                    walk(body, dec, seen, out);
                } else {
                    *out.entry(task.clone()).or_default() += 1;
                }
            }
            Node::Loop { var, ctl, body } => {
                for _ in 0..dec.loops.get(var).copied().unwrap_or(1) {
                    walk(body, dec, seen, out);
                    *out.entry(ctl.clone()).or_default() += 1;
                }
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, dec, &mut BTreeMap::new(), &mut out);
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
            Node::Sub { body, .. } | Node::MsgBoundary { body, .. } => walk(body, out),
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
    /// The two sides of the message-boundary race, counted so a sweep can
    /// prove it went both ways instead of assuming it did.
    pub delivered: usize,
    pub hosts_completed: usize,
}

/// Drive the engine to completion under `dec`, acting on whichever open work
/// item `rng` picks — so interleaving varies while the outcome must not.
/// Returns how many times the *engine* actually ran each element.
///
/// "Acting on" is where the message boundary enters: an item whose element
/// hosts one is either completed or taken away by its message, as `dec`'s
/// schedule says. Delivery is checked, not assumed — the host's work item must
/// come back closed and the step must have said `work-item-cancelled`.
pub fn run(
    proc: &ExecutableProcess,
    root: &Node,
    dec: &Decisions,
    rng: &mut Rng,
    step_budget: usize,
) -> Result<Run, String> {
    let controls = control_tasks(root);
    let hosts = boundary_hosts(root);
    // Activations per boundary, the index into its delivery schedule. The
    // oracle counts the same way, and a boundary lives in exactly one branch,
    // so interleaving cannot make the two disagree about the order.
    let mut activations: BTreeMap<String, usize> = BTreeMap::new();
    let (mut delivered, mut hosts_completed) = (0usize, 0usize);
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

        if let Some(boundary) = hosts.get(&element) {
            let activation = activations.entry(boundary.clone()).or_default();
            let deliver = dec.delivers(boundary, *activation);
            *activation += 1;
            if deliver {
                let Some(sub) = state
                    .subscriptions()
                    .find(|(_, s)| proc.node_id(s.element) == boundary)
                    .map(|(sid, _)| sid)
                else {
                    return Err(format!(
                        "no armed subscription at boundary '{boundary}' while its \
                         host '{element}' is open — the boundary never armed"
                    ));
                };
                *executions.entry(boundary.clone()).or_default() += 1;
                let events = step(
                    proc,
                    &mut state,
                    Command::DeliverMessage {
                        id: sub,
                        patch: json!({}),
                    },
                )
                .map_err(|e| format!("delivering to {boundary}: {e}"))?;
                let cancelled = format!("work-item-cancelled {element}");
                if !events.iter().any(|e| e.to_string() == cancelled) {
                    return Err(format!(
                        "delivering to '{boundary}' did not cancel host '{element}': {}",
                        events
                            .iter()
                            .map(|e| e.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                match state
                    .work_items()
                    .find(|(w, _)| *w == id)
                    .map(|(_, w)| w.open)
                {
                    Some(false) => {}
                    Some(true) => {
                        return Err(format!(
                            "host '{element}' still has an open work item after \
                             '{boundary}' interrupted it"
                        ));
                    }
                    None => {
                        return Err(format!(
                            "host '{element}' work item vanished when '{boundary}' fired \
                             — a cancelled item must stay, closed, to answer a late caller"
                        ));
                    }
                }
                delivered += 1;
                continue;
            }
            hosts_completed += 1;
        }

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
        delivered,
        hosts_completed,
    })
}
