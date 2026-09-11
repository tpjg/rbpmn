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
//!
//! `SideBoundary` is the one production that breaks that note, and it is
//! required to: a non-interrupting boundary spawns a *sibling* token, and
//! `boundary-side-path` says a side path must end at an end event of its own
//! and never merge into anything. It stays out of `end-event-in-branch`'s way
//! for a different reason than every other block — the region analysis walks
//! only *interrupting* pseudo-edges, so a side path is not part of any branch
//! (`accept/37-side-path-inside-a-parallel-block.bpmn` is the fixture that
//! makes that necessary rather than merely stated).

// Shared by several test binaries (`generator.rs`, `explore.rs`), each of
// which uses a different part of it.
#![allow(dead_code)]

use rbpmn_core::*;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

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
    /// A user task carrying a **non-interrupting** message boundary. Every
    /// delivery leaves the host exactly as it was and spawns a sibling token
    /// that runs the wrapped block and ends at an **end event of its own**
    /// — `boundary-side-path`: a side path may not merge back into the host's
    /// continuation (the rest of the process would run once per delivery)
    /// and may not reach a parallel join (which would collect a second token
    /// on one incoming flow). So, unlike every other production, the block's
    /// exit is simply the host: there is nothing to merge.
    ///
    /// **The body carries no boundary of its own, by construction** (see the
    /// strategies in `tests/generator.rs` and `tests/mutation.rs`). Two side
    /// tokens can be alive at once, so a message arm inside the path would be
    /// armed twice on the same `(message, key)` — an instance-wide duplicate,
    /// which the core refuses by freezing. That is the engine being right;
    /// generating it would only mean generating models that cannot complete.
    /// Loops, exclusive and parallel blocks and subprocesses inside a side
    /// path are all fine and are generated: the driver's loop budget is a
    /// count of *completions*, so two concurrent instances of one loop still
    /// total what the oracle predicts.
    SideBoundary(Box<Block>),
    /// An embedded subprocess wrapping a whole block. Semantically a no-op —
    /// `Sub(B)` executes exactly what `B` does — which is what makes it a
    /// sharp oracle test: any scope bookkeeping that leaks into execution
    /// shows up as a task count that no longer matches the plain block.
    Sub(Box<Block>),
    /// A **service task** carrying interrupting *error* boundaries, whose path
    /// runs the wrapped block and merges back as `MsgBoundary`'s does. `catch`
    /// says which boundaries: one for the host's own code, a catch-all (no
    /// `errorRef`), or both — where the exact code must win. `scoped` puts
    /// them on a one-task subprocess around the host instead, so the failure
    /// walks outward one scope and tears it down before a boundary takes it
    /// (docs/design/incident-scope.md, D1).
    ///
    /// The scope holds the host alone, deliberately: a failure in a scope with
    /// concurrent work cancels that work, and how much of it had already run
    /// is the interleaving's to decide — no count-based oracle can predict
    /// it. That case is the explorer's (`tests/explore.rs`), which sees every
    /// interleaving and every failure code.
    ErrBoundary {
        catch: Catch,
        scoped: bool,
        body: Box<Block>,
    },
}

/// Which error boundaries an `ErrBoundary` host carries.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Catch {
    /// One boundary, for the host's own error code.
    Coded,
    /// One boundary with no `errorRef`: it takes any failure, coded or not.
    All,
    /// Both on one host. A failure with the host's code must take the coded
    /// one and anything else the catch-all — precedence within one activity.
    Both,
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
    /// Host task, its **non-interrupting** message boundary, the side path,
    /// and the side path's own end event. No merge gateway: the side token
    /// is consumed at `end`, and the block's exit is `task` itself.
    SideBoundary {
        /// The host: a user task, parked on a work item that every delivery
        /// must leave untouched — that is what non-interrupting means, and
        /// the driver checks it at every delivery rather than assuming it.
        task: String,
        /// The `boundaryEvent` (`cancelActivity="false"`); also the element
        /// the correlation binding is registered under.
        boundary: String,
        /// The `bpmn:message` element id, unique per boundary. Re-arming
        /// makes that matter more than it does for an interrupting boundary:
        /// the same `(message, key)` is armed again on every delivery, so
        /// sharing one across boundaries would be a duplicate in waiting.
        message: String,
        body: Box<Node>,
        /// The side path's own `endEvent` — where the sibling token is
        /// consumed, and the reason `boundary-side-path` accepts the shape.
        end: String,
    },
    Sub {
        /// The `subProcess` element; also the scope's owner.
        id: String,
        /// The scope's own start and end events.
        start: String,
        end: String,
        body: Box<Node>,
    },
    /// Host service task, its error boundaries, and the path they share.
    ErrBoundary {
        /// The service task that fails — or completes.
        task: String,
        /// `(subProcess, start, end)` when the boundaries sit one scope out.
        scope: Option<(String, String, String)>,
        /// The boundary whose path runs `body`: the coded one for `Coded` and
        /// `Both`, the catch-all for `All`.
        boundary: String,
        /// The catch-all beside a coded boundary (`Both` only). Its path is
        /// empty and goes straight to the merge.
        any: Option<String>,
        catch: Catch,
        /// The host's own error code — declared as a `bpmn:error` unless the
        /// host carries only a catch-all.
        code: String,
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
        Block::SideBoundary(body) => {
            ids.boundaries += 1;
            ids.task += 1;
            let n = ids.boundaries;
            Node::SideBoundary {
                task: format!("t{}", ids.task),
                boundary: format!("b{n}"),
                message: format!("msg{n}"),
                body: Box::new(number(body, ids)),
                end: format!("b{n}_end"),
            }
        }
        Block::ErrBoundary {
            catch,
            scoped,
            body,
        } => {
            ids.boundaries += 1;
            ids.task += 1;
            let n = ids.boundaries;
            let task = format!("t{}", ids.task);
            let scope = (*scoped).then(|| {
                ids.subs += 1;
                let s = ids.subs;
                (
                    format!("sp{s}"),
                    format!("sp{s}_start"),
                    format!("sp{s}_end"),
                )
            });
            Node::ErrBoundary {
                task,
                scope,
                boundary: format!("b{n}"),
                any: (*catch == Catch::Both).then(|| format!("b{n}_any")),
                catch: *catch,
                code: format!("E{n}"),
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
    /// Message boundary; see `Element::boundary` for its host, the message it
    /// catches, and whether it interrupts. Interrupting-ness rides there
    /// rather than here for the reason stated above: a mutation swaps *kinds*
    /// in place, and it has no business turning a boundary's host into a
    /// second message id.
    MessageBoundary,
    /// The host of an `ErrBoundary`: error boundaries attach only to service
    /// tasks and subprocesses, and every other generated task is a user task.
    ServiceTask,
    /// Error boundary; see `Element::boundary` for its host and
    /// `BoundaryRefs::error_ref` for what it catches.
    ErrorBoundary,
}

/// The host, message and `cancelActivity` of a `Kind::MessageBoundary`.
#[derive(Clone, Debug)]
pub struct BoundaryRefs {
    pub attached_to: String,
    /// The `bpmn:message` element id, not its name.
    pub message: String,
    /// `false` emits `cancelActivity="false"` — the boundary spawns a sibling
    /// token onto a side path and leaves the host running. `true` omits the
    /// attribute entirely, which is what every accepted interrupting fixture
    /// does and what the default already means.
    pub interrupting: bool,
    /// `Kind::ErrorBoundary` only: the `bpmn:error` it references, or `None`
    /// for a catch-all. (`message` is empty on an error boundary.)
    pub error_ref: Option<String>,
}

/// A `bpmn:message` root element. Its **name** is what `correlate()` addresses
/// and what the trace prints; the id is only what `messageRef` points at.
#[derive(Clone, Debug)]
pub struct Message {
    pub id: String,
    pub name: String,
}

/// A `bpmn:error` root element: what a coded error boundary's `errorRef`
/// points at, and the code a failure must carry to match it.
#[derive(Clone, Debug)]
pub struct ErrorDef {
    pub id: String,
    pub code: String,
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
    /// `bpmn:error` root elements, one per coded error boundary.
    pub errors: Vec<ErrorDef>,
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
    fn boundary_element(&mut self, id: &str, attached_to: &str, message: &str, interrupting: bool) {
        self.elements.push(Element {
            id: id.to_string(),
            kind: Kind::MessageBoundary,
            container: self.container.clone(),
            boundary: Some(BoundaryRefs {
                attached_to: attached_to.to_string(),
                message: message.to_string(),
                interrupting,
                error_ref: None,
            }),
        });
    }

    /// An error boundary: interrupting by definition, reached through its
    /// host's failure rather than through the graph, and catching the error
    /// `error_ref` names — or, with none, anything.
    fn error_boundary_element(&mut self, id: &str, attached_to: &str, error_ref: Option<&str>) {
        self.elements.push(Element {
            id: id.to_string(),
            kind: Kind::ErrorBoundary,
            container: self.container.clone(),
            boundary: Some(BoundaryRefs {
                attached_to: attached_to.to_string(),
                message: String::new(),
                interrupting: true,
                error_ref: error_ref.map(str::to_string),
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
                self.boundary_element(boundary, task, message, true);
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
            Node::SideBoundary {
                task,
                boundary,
                message,
                body,
                end,
            } => {
                self.element(task, Kind::UserTask);
                self.flow(from, task, cond);
                self.messages.push(Message {
                    id: message.clone(),
                    name: message.to_uppercase(),
                });
                self.boundary_element(boundary, task, message, false);
                // The side path, run to an end event of its own. Nothing
                // merges: the sibling token is consumed here, and the block's
                // exit is the host, which still has exactly one outgoing flow
                // — so an enclosing parallel branch delivers one token to its
                // join exactly as a plain task would.
                let handled = self.add(body, boundary, None);
                self.element(end, Kind::End);
                self.flow(&handled, end, None);
                task.clone()
            }
            Node::ErrBoundary {
                task,
                scope,
                boundary,
                any,
                catch,
                code,
                body,
            } => {
                // The host: a service task, alone inside a subprocess when the
                // boundaries sit one scope out — the subprocess is then what
                // they attach to, and the failure reaches them by walking out.
                let host = match scope {
                    None => {
                        self.element(task, Kind::ServiceTask);
                        self.flow(from, task, cond);
                        task.clone()
                    }
                    Some((sp, start, end)) => {
                        self.element(sp, Kind::SubProcess);
                        self.flow(from, sp, cond);
                        let outer = self.container.replace(sp.clone());
                        self.element(start, Kind::Start);
                        self.element(task, Kind::ServiceTask);
                        self.flow(start, task, None);
                        self.element(end, Kind::End);
                        self.flow(task, end, None);
                        self.container = outer;
                        sp.clone()
                    }
                };
                let error_id = format!("err_{boundary}");
                if *catch != Catch::All {
                    self.errors.push(ErrorDef {
                        id: error_id.clone(),
                        code: code.clone(),
                    });
                }
                let main_ref = (*catch != Catch::All).then_some(error_id.as_str());
                self.error_boundary_element(boundary, &host, main_ref);
                // Every exit merges, as `MsgBoundary`'s do and for the same
                // reason: exactly one is ever taken, and the block must leave
                // by a single element.
                let mut exits = vec![host.clone(), self.add(body, boundary, None)];
                if let Some(any) = any {
                    self.error_boundary_element(any, &host, None);
                    exits.push(any.clone());
                }
                let n = self.elements.len();
                let merge = format!("em{n}");
                self.element(&merge, Kind::Exclusive);
                for exit in exits {
                    self.flow(&exit, &merge, None);
                }
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
        let errors: String = self
            .errors
            .iter()
            .map(|e| {
                format!(
                    "  <bpmn:error id=\"{}\" name=\"{}\" errorCode=\"{}\" />\n",
                    e.id, e.code, e.code
                )
            })
            .collect();
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <bpmn:definitions xmlns:bpmn=\"http://www.omg.org/spec/BPMN/20100524/MODEL\" \
             id=\"defs\" targetNamespace=\"https://rbpmn.dev/generated\">\n{messages}{errors}\
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
            if e.kind == Kind::ErrorBoundary {
                let b = e.boundary.as_ref().expect("a boundary knows its host");
                let def = match &b.error_ref {
                    Some(r) => format!("<bpmn:errorEventDefinition errorRef=\"{r}\" />"),
                    None => "<bpmn:errorEventDefinition />".to_string(),
                };
                out.push_str(&format!(
                    "    <bpmn:boundaryEvent id=\"{}\" attachedToRef=\"{}\">{inc}{out_tags}{def}\
                     </bpmn:boundaryEvent>\n",
                    e.id, b.attached_to
                ));
                continue;
            }
            if e.kind == Kind::MessageBoundary {
                let b = e.boundary.as_ref().expect("a boundary knows its host");
                // `{inc}` is always empty by construction; it is emitted so a
                // mutation that points a flow *at* a boundary produces the
                // invalid document the linter should refuse, not a silently
                // dropped edge.
                // `cancelActivity` is written only when it is false: the
                // attribute defaults to true, and spelling the default out
                // would make every interrupting fixture and every generated
                // model disagree on a detail that means nothing.
                let cancel = if b.interrupting {
                    ""
                } else {
                    " cancelActivity=\"false\""
                };
                out.push_str(&format!(
                    "    <bpmn:boundaryEvent id=\"{}\"{cancel} attachedToRef=\"{}\">{inc}{out_tags}\
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
                Kind::ServiceTask => ("bpmn:serviceTask", String::new()),
                Kind::Parallel => ("bpmn:parallelGateway", String::new()),
                Kind::Inclusive => ("bpmn:inclusiveGateway", String::new()),
                Kind::SubProcess | Kind::MessageBoundary | Kind::ErrorBoundary => {
                    unreachable!("handled above")
                }
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
    // A non-interrupting boundary needs the same manifest entry, and needs it
    // to keep working: the arm is re-evaluated against the document on every
    // delivery, so a key that resolves once must resolve every time.
    for host in side_boundary_hosts(&root).into_values() {
        bindings = bindings.correlation(host.boundary, CORRELATION_NAME);
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
            Node::SideBoundary { body, .. } | Node::ErrBoundary { body, .. } => walk(body, out),
        }
    }
    let mut out = BTreeMap::new();
    walk(root, &mut out);
    out
}

/// Everything the driver needs to know about one non-interrupting boundary,
/// looked up by its host task's id.
#[derive(Debug, Clone)]
pub struct SideHost {
    /// The `boundaryEvent` — where the subscription is armed, re-armed, and
    /// finally withdrawn.
    pub boundary: String,
    /// The message's **name** (what a trace line prints), not its element id.
    pub message: String,
    /// Every element on the side path that can hold a work item. "A delivery
    /// started the side path" is checked against this set rather than against
    /// one named task, because the body may be a parallel block (several new
    /// items), an exclusive one (which of them depends on the decisions) or a
    /// subprocess (the item is inside the child scope).
    pub body: BTreeSet<String>,
    /// The side path's own end event — for an empty body, the only thing that
    /// happens on a delivery.
    pub end: String,
}

/// Host task id -> the non-interrupting boundary armed on it. The map the
/// driver reads to answer "this open item is a side-boundary host: does its
/// schedule still owe a delivery?".
pub fn side_boundary_hosts(root: &Node) -> BTreeMap<String, SideHost> {
    fn walk(node: &Node, out: &mut BTreeMap<String, SideHost>) {
        match node {
            Node::Task(_) => {}
            Node::Seq(parts) | Node::Par(parts) => parts.iter().for_each(|p| walk(p, out)),
            Node::Xor { branches, .. } => branches.iter().for_each(|b| walk(b, out)),
            Node::Sub { body, .. }
            | Node::Loop { body, .. }
            | Node::MsgBoundary { body, .. }
            | Node::ErrBoundary { body, .. } => walk(body, out),
            Node::SideBoundary {
                task,
                boundary,
                message,
                body,
                end,
            } => {
                out.insert(
                    task.clone(),
                    SideHost {
                        boundary: boundary.clone(),
                        message: message.to_uppercase(),
                        body: work_item_elements(body),
                        end: end.clone(),
                    },
                );
                walk(body, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, &mut out);
    out
}

/// Every element in a subtree that parks a token on a work item. Gateways,
/// subprocesses, start and end events are not among them — nothing waits
/// there — so this is exactly "which elements could a new open item name".
pub fn work_item_elements(node: &Node) -> BTreeSet<String> {
    fn walk(node: &Node, out: &mut BTreeSet<String>) {
        match node {
            Node::Task(id) => {
                out.insert(id.clone());
            }
            Node::Seq(parts) | Node::Par(parts) => parts.iter().for_each(|p| walk(p, out)),
            Node::Xor { branches, .. } => branches.iter().for_each(|b| walk(b, out)),
            Node::Sub { body, .. } => walk(body, out),
            Node::Loop { ctl, body, .. } => {
                out.insert(ctl.clone());
                walk(body, out);
            }
            Node::MsgBoundary { task, body, .. }
            | Node::SideBoundary { task, body, .. }
            | Node::ErrBoundary { task, body, .. } => {
                out.insert(task.clone());
                walk(body, out);
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(node, &mut out);
    out
}

/// Everything the driver needs about one error-boundary host, by host task id.
#[derive(Debug, Clone)]
pub struct ErrHost {
    pub boundary: String,
    pub any: Option<String>,
    pub catch: Catch,
    pub code: String,
}

/// Host task id -> its error boundaries. The driver's map from "an open work
/// item turned up" to "its schedule may say to fail it instead".
pub fn error_hosts(root: &Node) -> BTreeMap<String, ErrHost> {
    fn walk(node: &Node, out: &mut BTreeMap<String, ErrHost>) {
        match node {
            Node::Task(_) => {}
            Node::Seq(parts) | Node::Par(parts) => parts.iter().for_each(|p| walk(p, out)),
            Node::Xor { branches, .. } => branches.iter().for_each(|b| walk(b, out)),
            Node::Sub { body, .. }
            | Node::Loop { body, .. }
            | Node::MsgBoundary { body, .. }
            | Node::SideBoundary { body, .. } => walk(body, out),
            Node::ErrBoundary {
                task,
                boundary,
                any,
                catch,
                code,
                body,
                ..
            } => {
                out.insert(
                    task.clone(),
                    ErrHost {
                        boundary: boundary.clone(),
                        any: any.clone(),
                        catch: *catch,
                        code: code.clone(),
                    },
                );
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
    /// Per **non-interrupting** boundary, one count per activation of its
    /// host: how many messages arrive before the host completes, 0..=
    /// [`MAX_SIDE_DELIVERIES`]. A count rather than a flag because the
    /// boundary re-arms — the host is never observably without it, so there
    /// is no "it fired, that was that" to record.
    pub side: BTreeMap<String, Vec<usize>>,
    /// Per error boundary, one outcome per activation of its host: complete,
    /// or fail — with the host's own code, another, or none. A failure is
    /// scheduled only where a boundary on the host takes it, at once or once
    /// a repair diverts the host's own code into it ([`Catch::outcomes`]), so
    /// every run still completes and the oracle has an answer for it.
    pub fail: BTreeMap<String, Vec<HostOutcome>>,
}

/// How many deliveries one activation of a non-interrupting boundary may
/// receive. The same bound, and the same reason, as the explorer's
/// `MAX_SIDE_TOKENS`: a re-arming boundary can spawn side tokens without
/// limit, so a schedule that is not bounded describes an infinite run rather
/// than a test. Two is what the mechanism needs — a second sibling proves the
/// re-arm is real and that two side tokens coexist; a third only makes the
/// same run longer.
pub const MAX_SIDE_DELIVERIES: usize = 2;

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

    /// How many messages `boundary`'s `n`-th activation receives. Past the end
    /// of the schedule the answer is "none, complete the host". As with
    /// [`Self::delivers`], the oracle and the driver both read the schedule
    /// through *this* function, so they cannot disagree about what a missing
    /// entry means.
    pub fn deliveries(&self, boundary: &str, activation: usize) -> usize {
        self.side
            .get(boundary)
            .and_then(|s| s.get(activation))
            .copied()
            .unwrap_or(0)
    }

    /// What `boundary`'s host does on its `n`-th activation. Past the end of
    /// the schedule it completes — read through here by both the oracle and
    /// the driver, like [`Self::delivers`].
    pub fn host_outcome(&self, boundary: &str, activation: usize) -> HostOutcome {
        self.fail
            .get(boundary)
            .and_then(|s| s.get(activation))
            .copied()
            .unwrap_or(HostOutcome::Completes)
    }
}

/// What one activation of an error-boundary host does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum HostOutcome {
    Completes,
    /// Fails with the host's own code — the one its coded boundary declares.
    FailsWithItsCode,
    /// Fails with a code no boundary declares.
    FailsWithAnotherCode,
    /// Fails with no code at all: the shape only a catch-all can take.
    FailsWithNoCode,
    /// Fails with no code on a coded-only host: nothing catches it, the
    /// instance freezes, and a repair diverts the host's own code into its
    /// boundary (docs/design/incident-scope.md, D5) — which must land exactly
    /// where `FailsWithItsCode` would have.
    FrozenThenDiverted,
}

/// Which exit of an error-boundary host an outcome takes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Taker {
    Host,
    /// `ErrBoundary::boundary`, whose path runs the wrapped block.
    Boundary,
    /// A `Both` host's catch-all.
    CatchAll,
}

impl Catch {
    /// The outcomes worth scheduling: a completion, and every failure some
    /// boundary on this host takes. A coded-only host fails uncaught only as
    /// `FrozenThenDiverted`: the incident is raised, then repaired by
    /// diverting the host's own code, so the run still completes.
    pub fn outcomes(self) -> &'static [HostOutcome] {
        use HostOutcome::*;
        match self {
            Catch::Coded => &[Completes, FailsWithItsCode, FrozenThenDiverted],
            Catch::All => &[Completes, FailsWithAnotherCode, FailsWithNoCode],
            Catch::Both => &[
                Completes,
                FailsWithItsCode,
                FailsWithAnotherCode,
                FailsWithNoCode,
            ],
        }
    }

    /// The oracle's answer to "which boundary takes this": the exact code
    /// before the catch-all, on one host. The driver never consults it — it
    /// counts what the engine started — which is what makes it a prediction.
    pub fn taker(self, outcome: HostOutcome) -> Taker {
        match (self, outcome) {
            (_, HostOutcome::Completes) => Taker::Host,
            (Catch::Coded | Catch::Both, HostOutcome::FailsWithItsCode) => Taker::Boundary,
            (Catch::Coded, HostOutcome::FrozenThenDiverted) => Taker::Boundary,
            (Catch::All, _) => Taker::Boundary,
            (Catch::Both, _) => Taker::CatchAll,
            (Catch::Coded, other) => {
                panic!("a coded-only host is never scheduled to fail with {other:?}")
            }
        }
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
            Node::ErrBoundary {
                boundary,
                catch,
                body,
                ..
            } => {
                let choices = catch.outcomes();
                let schedule = (0..reps)
                    .map(|_| choices[rng.below(choices.len())])
                    .collect();
                out.fail.insert(boundary.clone(), schedule);
                walk(body, rng, max, reps, out);
            }
            Node::MsgBoundary { boundary, body, .. } => {
                let schedule = (0..reps).map(|_| rng.below(2) == 0).collect();
                out.deliver.insert(boundary.clone(), schedule);
                walk(body, rng, max, reps, out);
            }
            Node::SideBoundary { boundary, body, .. } => {
                let schedule = (0..reps)
                    .map(|_| rng.below(MAX_SIDE_DELIVERIES + 1))
                    .collect();
                out.side.insert(boundary.clone(), schedule);
                // The side path runs once per delivery, so an activation of
                // anything inside it is multiplied by the bound rather than
                // by one. Nothing in the generated grammar puts a boundary
                // there, but a schedule that is too short is a silent
                // "complete the host" and this is the line that would hide it.
                walk(body, rng, max, reps * MAX_SIDE_DELIVERIES, out);
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
            // The mirror image of `MsgBoundary`, and the reason both are
            // worth having: here the host **always** counts — the deliveries
            // did not touch it — and the boundary and its path count once
            // *per delivery*. Counting the host once and the path once would
            // be the interrupting answer; counting the path once for two
            // deliveries would be "it fired, that was that", i.e. no re-arm.
            Node::SideBoundary {
                task,
                boundary,
                body,
                ..
            } => {
                let activation = seen.entry(boundary.clone()).or_default();
                let deliveries = dec.deliveries(boundary, *activation);
                *activation += 1;
                *out.entry(task.clone()).or_default() += 1;
                for _ in 0..deliveries {
                    *out.entry(boundary.clone()).or_default() += 1;
                    walk(body, dec, seen, out);
                }
            }
            // `MsgBoundary`'s race with a failure in place of the message: the
            // host counts when it completes, and otherwise the boundary that
            // takes the failure counts — the coded one (or a lone catch-all)
            // with its path, or a `Both` host's catch-all with its empty one.
            // The driver counts what the engine *started*, so a core that let
            // the catch-all win over an exact code comes out a multiset apart.
            Node::ErrBoundary {
                task,
                boundary,
                any,
                catch,
                body,
                ..
            } => {
                let activation = seen.entry(boundary.clone()).or_default();
                let outcome = dec.host_outcome(boundary, *activation);
                *activation += 1;
                match catch.taker(outcome) {
                    Taker::Host => *out.entry(task.clone()).or_default() += 1,
                    Taker::Boundary => {
                        *out.entry(boundary.clone()).or_default() += 1;
                        walk(body, dec, seen, out);
                    }
                    Taker::CatchAll => {
                        let any = any
                            .as_ref()
                            .expect("only a Both host has a catch-all beside");
                        *out.entry(any.clone()).or_default() += 1;
                    }
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
            Node::Sub { body, .. }
            | Node::MsgBoundary { body, .. }
            | Node::SideBoundary { body, .. }
            | Node::ErrBoundary { body, .. } => walk(body, out),
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

/// What the repair production did, counted from what the engine did — never
/// from what was scheduled (docs/design/incident-scope.md, D5).
#[derive(Debug, Default, Clone)]
pub struct Repairs {
    pub frozen: usize,
    pub retried: usize,
    pub advanced: usize,
    pub diverted: usize,
    /// A sibling's completion refused while the instance was frozen, with
    /// nothing changed: `FreezeAdvancesNothing` at the core.
    pub siblings_refused: usize,
    /// A repair naming any incident but the open one, refused (D9).
    pub stale_refused: usize,
}

impl Repairs {
    pub fn add(&mut self, other: &Repairs) {
        self.frozen += other.frozen;
        self.retried += other.retried;
        self.advanced += other.advanced;
        self.diverted += other.diverted;
        self.siblings_refused += other.siblings_refused;
        self.stale_refused += other.stale_refused;
    }
}

/// Fail `id` with nothing to catch it, and check the freeze: the incident is
/// the next number this run minted, a sibling's completion is refused with
/// nothing changed, and so is a repair naming any other incident. Returns the
/// open incident, for the repair the caller makes.
fn freeze_and_check(
    proc: &ExecutableProcess,
    state: &mut InstanceState,
    id: WorkItemId,
    element: &str,
    open: &[(WorkItemId, String)],
    tally: &mut Repairs,
) -> Result<u64, String> {
    let events = step(proc, state, Command::RaiseError { id, code: None })
        .map_err(|e| format!("failing {element}: {e}"))?;
    let trace: Vec<String> = events.iter().map(|e| e.to_string()).collect();
    if state.status != InstanceStatus::Failed
        || trace.last() != Some(&format!("incident-raised {element}"))
    {
        return Err(format!(
            "failing '{element}' with nothing to catch it did not freeze the instance: {}",
            trace.join(", ")
        ));
    }
    let incident = tally.frozen as u64;
    tally.frozen += 1;
    if state.open_incident() != Some(incident) {
        return Err(format!(
            "the instance froze on incident {:?}, not {incident} — each freeze mints the next",
            state.open_incident()
        ));
    }
    let frozen = state.clone();
    if let Some((sibling, at)) = open.iter().find(|(w, _)| *w != id) {
        match step(
            proc,
            state,
            Command::CompleteWorkItem {
                id: *sibling,
                patch: json!({}),
            },
        ) {
            Err(StepError::InstanceNotActive(InstanceStatus::Failed)) if *state == frozen => {
                tally.siblings_refused += 1;
            }
            other => {
                return Err(format!(
                    "completing '{at}' on an instance frozen at '{element}' answered {other:?}"
                ));
            }
        }
    }
    match step(
        proc,
        state,
        Command::Repair {
            incident: incident + 1,
            disposition: Disposition::Retry { patch: json!({}) },
            reason: "stale".to_string(),
        },
    ) {
        Err(StepError::IncidentNotOpen { .. }) if *state == frozen => tally.stale_refused += 1,
        other => {
            return Err(format!(
                "a repair of incident {} on an instance frozen at incident {incident} \
                 answered {other:?}",
                incident + 1
            ));
        }
    }
    Ok(incident)
}

pub struct Run {
    pub executions: BTreeMap<String, usize>,
    pub status: InstanceStatus,
    pub steps: usize,
    /// The two sides of the message-boundary race, counted so a sweep can
    /// prove it went both ways instead of assuming it did.
    pub delivered: usize,
    pub hosts_completed: usize,
    /// One entry per *completed* activation of a non-interrupting boundary
    /// host: how many messages that activation received. The histogram, not a
    /// total — "it delivered a lot" is compatible with never delivering zero
    /// and never delivering twice, and both of those are the interesting ends.
    pub side_deliveries: Vec<usize>,
    /// How often a side-boundary host completed while its side path still had
    /// an open work item. That is the design's §3.5 claim in one number: the
    /// instance stays active until the side work is done, and a run in which
    /// it never happened tested the claim not at all.
    pub hosts_completed_with_side_work: usize,
    /// Error-boundary hosts: completed, and each way a failure was taken —
    /// counted from the boundary the engine started, so a sweep can prove
    /// every exit happened rather than that it was scheduled.
    pub error_hosts_completed: usize,
    pub caught_by_code: usize,
    /// The coded boundary took it on a host that also carried a catch-all.
    pub exact_beat_catch_all: usize,
    pub caught_by_catch_all_with_code: usize,
    pub caught_by_catch_all_without_code: usize,
    pub repairs: Repairs,
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
    let side_hosts = side_boundary_hosts(root);
    let err_hosts = error_hosts(root);
    let mut err_activations: BTreeMap<String, usize> = BTreeMap::new();
    let (mut error_hosts_completed, mut caught_by_code, mut exact_beat_catch_all) =
        (0usize, 0usize, 0usize);
    let (mut caught_by_catch_all_with_code, mut caught_by_catch_all_without_code) =
        (0usize, 0usize);
    let mut repairs = Repairs::default();
    // Per non-interrupting boundary: which activation of its host we are in,
    // and how many of that activation's deliveries have been made. One
    // delivery per driver step, like every other unit of work, so the
    // interleaving stays random between them.
    let mut side_progress: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut side_deliveries: Vec<usize> = Vec::new();
    let mut hosts_completed_with_side_work = 0usize;
    // Activations per boundary, the index into its delivery schedule. The
    // oracle counts the same way, and a boundary lives in exactly one branch,
    // so interleaving cannot make the two disagree about the order.
    let mut activations: BTreeMap<String, usize> = BTreeMap::new();
    let (mut delivered, mut hosts_completed) = (0usize, 0usize);
    // Iterations still owed per loop, refilled when the loop exits — which is
    // what makes nested loops come out right on re-entry. It is a count of
    // *completions*, not of tokens, and that is what makes it survive a loop
    // inside a side path, where two side tokens can be going round at once:
    // an exit is handed out every n completions, so E entries cost exactly
    // E x n completions however they interleave — which is the multiset the
    // oracle predicts. (Per-token budgets would be wrong as often as right:
    // the loop's variable is one document entry both tokens read.)
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

        // A non-interrupting boundary's host. Every delivery its schedule
        // still owes is one driver step of its own, and every one of them is
        // *checked*: the host untouched, the boundary re-armed under a new
        // id, and the side path started. None of that is assumed anywhere
        // else — the oracle only ever sees a multiset.
        if let Some(host) = side_hosts.get(&element) {
            let progress = side_progress.entry(host.boundary.clone()).or_insert((0, 0));
            let want = dec.deliveries(&host.boundary, progress.0);
            if progress.1 < want {
                progress.1 += 1;
                let armed = |state: &InstanceState| {
                    state
                        .subscriptions()
                        .find(|(_, s)| proc.node_id(s.element) == host.boundary)
                        .map(|(sid, _)| sid)
                };
                let Some(sub) = armed(&state) else {
                    return Err(format!(
                        "no armed subscription at non-interrupting boundary '{}' while \
                         its host '{element}' is open — it never armed, or it did not \
                         re-arm after the previous delivery",
                        host.boundary
                    ));
                };
                let before: BTreeMap<WorkItemId, String> = open.iter().cloned().collect();
                *executions.entry(host.boundary.clone()).or_default() += 1;
                let events = step(
                    proc,
                    &mut state,
                    Command::DeliverMessage {
                        id: sub,
                        patch: json!({}),
                    },
                )
                .map_err(|e| format!("delivering to {}: {e}", host.boundary))?;
                let trace: Vec<String> = events.iter().map(|e| e.to_string()).collect();

                // Non-interrupting, stated as two independent checks: the
                // host's item is still open, and nothing cancelled it. The
                // first is the state, the second is the record a consumer
                // would see; an implementation that cancelled and re-created
                // would pass one and fail the other.
                if state
                    .work_items()
                    .find(|(w, _)| *w == id)
                    .map(|(_, w)| w.open)
                    != Some(true)
                {
                    return Err(format!(
                        "'{}' is non-interrupting but its host '{element}' no longer has \
                         an open work item after a delivery: {}",
                        host.boundary,
                        trace.join(", ")
                    ));
                }
                if trace
                    .iter()
                    .any(|e| *e == format!("work-item-cancelled {element}"))
                {
                    return Err(format!(
                        "'{}' cancelled its host '{element}' — cancelActivity=\"false\" \
                         says it must not",
                        host.boundary
                    ));
                }

                // Re-armed: a *new* subscription, not the old one surviving.
                match armed(&state) {
                    None => {
                        return Err(format!(
                            "'{}' did not re-arm after a delivery — its host '{element}' \
                             is still open and a live host is never without its boundary: {}",
                            host.boundary,
                            trace.join(", ")
                        ));
                    }
                    Some(again) if again == sub => {
                        return Err(format!(
                            "'{}' still holds subscription {sub:?} after delivering it — \
                             a re-arm is a new subscription, not a reused row",
                            host.boundary
                        ));
                    }
                    Some(_) => {}
                }

                // The side token: a sibling that started the side path. For a
                // path that is nothing but its end event there is no work item
                // to find, and the token ran straight to the end instead.
                let new_items: Vec<&str> = state
                    .open_work_items()
                    .filter(|(w, _)| !before.contains_key(w))
                    .map(|(_, w)| proc.node_id(w.element))
                    .collect();
                if host.body.is_empty() {
                    if !new_items.is_empty() {
                        return Err(format!(
                            "'{}' has an empty side path but a delivery opened {new_items:?}",
                            host.boundary
                        ));
                    }
                    if !trace
                        .iter()
                        .any(|e| *e == format!("element-completed {}", host.end))
                    {
                        return Err(format!(
                            "'{}' spawned a side token that never reached its end event \
                             '{}': {}",
                            host.boundary,
                            host.end,
                            trace.join(", ")
                        ));
                    }
                } else if new_items.is_empty() {
                    return Err(format!(
                        "'{}' delivered but no side token appeared on its path: {}",
                        host.boundary,
                        trace.join(", ")
                    ));
                } else if let Some(stray) = new_items.iter().find(|e| !host.body.contains(**e)) {
                    return Err(format!(
                        "delivering to '{}' opened a work item at '{stray}', which is not \
                         on its side path — a side token entered the host's own flow",
                        host.boundary
                    ));
                }
                continue;
            }
            // The schedule is spent: this activation ends by completing the
            // host, and the next one starts from the next schedule entry.
            side_deliveries.push(want);
            *progress = (progress.0 + 1, 0);
        }

        // An error-boundary host: complete it, or fail it the way its schedule
        // says. The boundary counted is the one the *engine* started — never
        // the one the oracle expects — so precedence is tested, not assumed.
        if let Some(host) = err_hosts.get(&element) {
            let activation = err_activations.entry(host.boundary.clone()).or_default();
            let outcome = dec.host_outcome(&host.boundary, *activation);
            *activation += 1;
            // A coded-only host's codeless failure: nothing catches it, the
            // instance freezes, and a repair diverts the host's own code —
            // which must land where `FailsWithItsCode` would have, walking out
            // to the enclosing subprocess for a scoped host (D5).
            if outcome == HostOutcome::FrozenThenDiverted {
                let incident =
                    freeze_and_check(proc, &mut state, id, &element, &open, &mut repairs)?;
                let events = step(
                    proc,
                    &mut state,
                    Command::Repair {
                        incident,
                        disposition: Disposition::Divert {
                            code: Some(host.code.clone()),
                        },
                        reason: "generated".to_string(),
                    },
                )
                .map_err(|e| format!("diverting {element}: {e}"))?;
                let trace: Vec<String> = events.iter().map(|e| e.to_string()).collect();
                if !trace
                    .iter()
                    .any(|e| *e == format!("element-started {}", host.boundary))
                {
                    return Err(format!(
                        "diverting '{}' at '{element}' did not take '{}': {}",
                        host.code,
                        host.boundary,
                        trace.join(", ")
                    ));
                }
                repairs.diverted += 1;
                *executions.entry(host.boundary.clone()).or_default() += 1;
                continue;
            }
            if outcome != HostOutcome::Completes {
                let code = match outcome {
                    HostOutcome::FailsWithItsCode => Some(host.code.clone()),
                    HostOutcome::FailsWithAnotherCode => Some(format!("{}_OTHER", host.code)),
                    _ => None,
                };
                let events = step(
                    proc,
                    &mut state,
                    Command::RaiseError {
                        id,
                        code: code.clone(),
                    },
                )
                .map_err(|e| format!("failing {element}: {e}"))?;
                let trace: Vec<String> = events.iter().map(|e| e.to_string()).collect();
                if trace.iter().any(|e| e.starts_with("incident-raised")) {
                    return Err(format!(
                        "failing '{element}' with {code:?} raised an incident although \
                         '{}' is on its host: {}",
                        host.boundary,
                        trace.join(", ")
                    ));
                }
                let taken = std::iter::once(&host.boundary)
                    .chain(host.any.iter())
                    .find(|b| trace.iter().any(|e| *e == format!("element-started {b}")))
                    .cloned();
                let Some(taken) = taken else {
                    return Err(format!(
                        "failing '{element}' with {code:?} started none of its boundaries: {}",
                        trace.join(", ")
                    ));
                };
                if state
                    .work_items()
                    .find(|(w, _)| *w == id)
                    .map(|(_, w)| w.open)
                    != Some(false)
                {
                    return Err(format!(
                        "'{element}' failed into '{taken}' but its work item is not closed"
                    ));
                }
                let catch_all_took_it = match host.catch {
                    Catch::All => true,
                    _ => host.any.as_ref() == Some(&taken),
                };
                match (catch_all_took_it, code.is_some()) {
                    (false, _) => {
                        caught_by_code += 1;
                        if host.any.is_some() {
                            exact_beat_catch_all += 1;
                        }
                    }
                    (true, true) => caught_by_catch_all_with_code += 1,
                    (true, false) => caught_by_catch_all_without_code += 1,
                }
                *executions.entry(taken).or_default() += 1;
                continue;
            }
            error_hosts_completed += 1;
        }

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

        // A plain task: now and then it fails with nothing to catch it, the
        // instance freezes, and a repair brings it back — Retry or Advance,
        // the two whose outcome the oracle already predicts (D5). A retried
        // task is completed later like any other, so this step completes
        // nothing; an advanced one counts as completed here.
        let plain = !hosts.contains_key(&element)
            && !side_hosts.contains_key(&element)
            && !err_hosts.contains_key(&element);
        let repair = plain && rng.below(4) == 0;
        if repair && rng.below(2) == 0 {
            let incident = freeze_and_check(proc, &mut state, id, &element, &open, &mut repairs)?;
            let events = step(
                proc,
                &mut state,
                Command::Repair {
                    incident,
                    disposition: Disposition::Retry { patch: json!({}) },
                    reason: "generated".to_string(),
                },
            )
            .map_err(|e| format!("retrying {element}: {e}"))?;
            let trace: Vec<String> = events.iter().map(|e| e.to_string()).collect();
            // A new item, not merely another open one at the same element:
            // two side tokens can both be waiting at one task.
            let reopened = state.open_work_items().any(|(w, item)| {
                proc.node_id(item.element) == element && !open.iter().any(|(o, _)| *o == w)
            });
            if trace.first() != Some(&format!("incident-repaired {element} retry"))
                || !reopened
                || state.status != InstanceStatus::Active
            {
                return Err(format!(
                    "retrying '{element}' did not open it again on an active instance: {}",
                    trace.join(", ")
                ));
            }
            repairs.retried += 1;
            continue;
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
        let events = if repair {
            let incident = freeze_and_check(proc, &mut state, id, &element, &open, &mut repairs)?;
            let events = step(
                proc,
                &mut state,
                Command::Repair {
                    incident,
                    disposition: Disposition::Advance {
                        patch,
                        answer: None,
                    },
                    reason: "generated".to_string(),
                },
            )
            .map_err(|e| format!("advancing {element}: {e}"))?;
            let trace: Vec<String> = events.iter().map(|e| e.to_string()).collect();
            if trace.first() != Some(&format!("incident-repaired {element} advance"))
                || !trace.contains(&format!("element-completed {element}"))
                || state.status == InstanceStatus::Failed
            {
                return Err(format!(
                    "advancing '{element}' did not complete it: {}",
                    trace.join(", ")
                ));
            }
            repairs.advanced += 1;
            events
        } else {
            step(proc, &mut state, Command::CompleteWorkItem { id, patch })
                .map_err(|e| format!("completing {element}: {e}"))?
        };

        if let Some(host) = side_hosts.get(&element) {
            // The arm goes with the host, and only the arm: side tokens
            // already spawned are independent and run to their own end.
            let cancelled = format!("subscription-cancelled {} {}", host.boundary, host.message);
            if !events.iter().any(|e| e.to_string() == cancelled) {
                return Err(format!(
                    "completing '{element}' did not withdraw '{}': {}",
                    host.boundary,
                    events
                        .iter()
                        .map(|e| e.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            // §3.5, asserted rather than argued: a side token is a token, so
            // the instance stays active until the side work is done. Nothing
            // else here would notice a core that completed the instance the
            // moment its main flow ran out.
            let side_open = state
                .open_work_items()
                .any(|(_, w)| host.body.contains(proc.node_id(w.element)));
            if side_open {
                hosts_completed_with_side_work += 1;
                if state.status != InstanceStatus::Active {
                    return Err(format!(
                        "host '{element}' completed with work still open on '{}'s side \
                         path, and the instance reported {:?} — a side token is a token, \
                         and nothing may complete while one is in flight",
                        host.boundary, state.status
                    ));
                }
            }
        }
    }

    Ok(Run {
        executions,
        status: state.status,
        steps,
        delivered,
        hosts_completed,
        side_deliveries,
        hosts_completed_with_side_work,
        error_hosts_completed,
        caught_by_code,
        exact_beat_catch_all,
        caught_by_catch_all_with_code,
        caught_by_catch_all_without_code,
        repairs,
    })
}
