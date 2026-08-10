//! The step function — the correctness core.
//!
//! A step takes a quiescent state and one command, advances every affected
//! token synchronously to its next wait position, and returns the events.
//! Deterministic by construction: tokens advance breadth-first in a FIFO
//! queue, split branches spawn in sequence-flow declaration order, and all
//! collections iterate in id order — the same inputs always produce the same
//! trace (what makes golden-log fixtures possible).
//!
//! Errors are returned before any mutation, except [`StepError::Invariant`],
//! which signals a bug in the engine (lint-clean models cannot trigger it)
//! and poisons the state.

use crate::compile::{ExecKind, ExecutableProcess, FlowIx, NodeIx};
use crate::event::Event;
use crate::merge_patch::merge_patch;
use crate::state::{
    InstanceState, InstanceStatus, Token, TokenId, WaitKind, WorkItemId, WorkItemState,
};
use serde_json::Value;
use std::collections::VecDeque;

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Start the instance with its initial variable document.
    Start { variables: Value },
    /// Complete an open work item, applying an RFC 7386 merge patch to the
    /// variables in the same step that advances the token.
    CompleteWorkItem { id: WorkItemId, patch: Value },
    /// A work item's retry budget is exhausted: raise the named error. A
    /// matching error boundary on the host interrupts the task and takes the
    /// boundary path; no match freezes the instance in the incident state.
    RaiseError {
        id: WorkItemId,
        code: Option<String>,
    },
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum StepError {
    #[error("instance is already started")]
    AlreadyStarted,
    #[error("instance is not active (status: {0:?})")]
    InstanceNotActive(InstanceStatus),
    #[error("no work item {0:?} in this instance")]
    UnknownWorkItem(WorkItemId),
    /// Distinct from unknown: the item existed and was completed or
    /// cancelled. Callers map this to their idempotent no-op response.
    #[error("work item {0:?} is not open (already completed or cancelled)")]
    WorkItemNotOpen(WorkItemId),
    #[error("internal invariant violated: {0} — state is poisoned")]
    Invariant(String),
}

pub fn step(
    proc: &ExecutableProcess,
    state: &mut InstanceState,
    command: Command,
) -> Result<Vec<Event>, StepError> {
    match command {
        Command::Start { variables } => {
            match state.status {
                InstanceStatus::Created => {}
                InstanceStatus::Active => return Err(StepError::AlreadyStarted),
                status => return Err(StepError::InstanceNotActive(status)),
            }
            state.status = InstanceStatus::Active;
            state.variables = variables;
            let mut adv = Advancer::new(proc);
            adv.events.push(Event::InstanceStarted);
            let token = state.next_token_id();
            adv.queue.push_back((token, proc.start(), None));
            adv.run(state)
        }
        Command::CompleteWorkItem { id, patch } => {
            if state.status != InstanceStatus::Active {
                return Err(StepError::InstanceNotActive(state.status));
            }
            let item = state
                .work_items
                .get(&id)
                .ok_or(StepError::UnknownWorkItem(id))?;
            if !item.open {
                return Err(StepError::WorkItemNotOpen(id));
            }
            let (element, token_id) = (item.element, item.token);

            state.work_items.get_mut(&id).unwrap().open = false;
            if state.tokens.remove(&token_id).is_none() {
                return Err(StepError::Invariant(format!(
                    "work item {id:?} referenced token {token_id:?} which does not exist"
                )));
            }

            let mut adv = Advancer::new(proc);
            adv.events.push(Event::WorkItemCompleted {
                id,
                element: proc.node_id(element).to_string(),
            });
            if patch != Value::Object(serde_json::Map::new()) {
                merge_patch(&mut state.variables, &patch);
                adv.events.push(Event::VariablesPatched { patch });
            }
            adv.events.push(Event::ElementCompleted {
                element: proc.node_id(element).to_string(),
            });
            adv.leave_single(state, token_id, element)?;
            adv.run(state)
        }
        Command::RaiseError { id, code } => {
            if state.status != InstanceStatus::Active {
                return Err(StepError::InstanceNotActive(state.status));
            }
            let item = state
                .work_items
                .get(&id)
                .ok_or(StepError::UnknownWorkItem(id))?;
            if !item.open {
                return Err(StepError::WorkItemNotOpen(id));
            }
            let (element, token_id) = (item.element, item.token);

            state.work_items.get_mut(&id).unwrap().open = false;
            let mut adv = Advancer::new(proc);
            adv.events.push(Event::WorkItemFailed {
                id,
                element: proc.node_id(element).to_string(),
                code: code.clone(),
            });

            let boundary = code
                .as_deref()
                .and_then(|c| proc.error_boundary(element, c));
            match boundary {
                Some(boundary_ix) => {
                    // Interrupting: the task's token continues on the
                    // boundary path.
                    if state.tokens.remove(&token_id).is_none() {
                        return Err(StepError::Invariant(format!(
                            "work item {id:?} referenced token {token_id:?} which does not exist"
                        )));
                    }
                    adv.element_started(boundary_ix);
                    adv.element_completed(boundary_ix);
                    adv.leave_single(state, token_id, boundary_ix)?;
                    adv.run(state)
                }
                None => {
                    // Incident: freeze everything as-is for repair.
                    state.status = InstanceStatus::Failed;
                    adv.events.push(Event::IncidentRaised {
                        element: proc.node_id(element).to_string(),
                        code,
                    });
                    Ok(adv.events)
                }
            }
        }
    }
}

struct Advancer<'a> {
    proc: &'a ExecutableProcess,
    events: Vec<Event>,
    queue: VecDeque<(TokenId, NodeIx, Option<FlowIx>)>,
}

impl<'a> Advancer<'a> {
    fn new(proc: &'a ExecutableProcess) -> Self {
        Advancer {
            proc,
            events: Vec::new(),
            queue: VecDeque::new(),
        }
    }

    fn run(mut self, state: &mut InstanceState) -> Result<Vec<Event>, StepError> {
        while let Some((token, node, via)) = self.queue.pop_front() {
            if state.status != InstanceStatus::Active {
                break;
            }
            self.enter(state, token, node, via)?;
        }
        if state.status == InstanceStatus::Active && state.tokens.is_empty() {
            state.status = InstanceStatus::Completed;
            self.events.push(Event::InstanceCompleted);
        }
        Ok(self.events)
    }

    fn enter(
        &mut self,
        state: &mut InstanceState,
        token: TokenId,
        node_ix: NodeIx,
        via: Option<FlowIx>,
    ) -> Result<(), StepError> {
        let node = self.proc.node(node_ix);
        match &node.kind {
            ExecKind::Start => {
                self.element_started(node_ix);
                self.element_completed(node_ix);
                self.leave_single(state, token, node_ix)
            }
            ExecKind::Task { kind, topic } => {
                self.element_started(node_ix);
                let item = state.alloc_work_item(WorkItemState {
                    element: node_ix,
                    token,
                    kind: *kind,
                    topic: topic.clone(),
                    open: true,
                });
                state.tokens.insert(
                    token,
                    Token {
                        node: node_ix,
                        wait: WaitKind::WorkItem(item),
                    },
                );
                self.events.push(Event::WorkItemCreated {
                    id: item,
                    element: node.id.clone(),
                    work_kind: *kind,
                    topic: topic.clone(),
                });
                Ok(())
            }
            ExecKind::ExclusiveGateway { default_flow } => {
                self.element_started(node_ix);
                let chosen = if node.outgoing.len() == 1 {
                    node.outgoing[0]
                } else {
                    let conditional =
                        node.outgoing
                            .iter()
                            .copied()
                            .filter(|f| Some(*f) != *default_flow)
                            .find(|f| {
                                self.proc.flow(*f).condition.as_ref().is_some_and(|c| {
                                    rbpmn_model::condition::eval(c, &state.variables)
                                })
                            });
                    match conditional.or(*default_flow) {
                        Some(flow) => flow,
                        None => {
                            return Err(StepError::Invariant(format!(
                                "exclusive split '{}' has no default flow",
                                node.id
                            )));
                        }
                    }
                };
                self.element_completed(node_ix);
                self.leave(state, token, chosen)
            }
            ExecKind::ParallelGateway => {
                if node.incoming.len() > 1 {
                    self.enter_join(state, token, node_ix, via)
                } else {
                    // Split (or degenerate pass-through): consume the token,
                    // spawn one per outgoing flow in declaration order.
                    self.element_started(node_ix);
                    self.element_completed(node_ix);
                    for &flow in &self.proc.node(node_ix).outgoing.clone() {
                        let child = state.next_token_id();
                        self.leave(state, child, flow)?;
                    }
                    Ok(())
                }
            }
            ExecKind::ErrorBoundary { .. } => Err(StepError::Invariant(format!(
                "error boundary '{}' entered via a sequence flow",
                node.id
            ))),
            ExecKind::End => {
                self.element_started(node_ix);
                self.element_completed(node_ix);
                // Token is consumed: it was never parked, simply not re-queued.
                Ok(())
            }
            ExecKind::TerminateEnd => {
                self.element_started(node_ix);
                self.element_completed(node_ix);
                let open: Vec<WorkItemId> = state
                    .work_items
                    .iter()
                    .filter(|(_, w)| w.open)
                    .map(|(id, _)| *id)
                    .collect();
                for id in open {
                    let item = state.work_items.get_mut(&id).unwrap();
                    item.open = false;
                    self.events.push(Event::WorkItemCancelled {
                        id,
                        element: self.proc.node_id(item.element).to_string(),
                    });
                }
                state.tokens.clear();
                self.queue.clear();
                state.status = InstanceStatus::Terminated;
                self.events.push(Event::InstanceTerminated);
                Ok(())
            }
        }
    }

    /// Parallel join: arrivals park silently; the join executes once, when
    /// every incoming flow holds a token — valid as *local* counting because
    /// `balanced-gateways` guarantees block structure.
    fn enter_join(
        &mut self,
        state: &mut InstanceState,
        token: TokenId,
        node_ix: NodeIx,
        via: Option<FlowIx>,
    ) -> Result<(), StepError> {
        let node = self.proc.node(node_ix);
        let via = via.ok_or_else(|| {
            StepError::Invariant(format!("join '{}' entered without a flow", node.id))
        })?;

        let arrived = |state: &InstanceState, flow: FlowIx| {
            state
                .tokens
                .iter()
                .find(|(_, t)| {
                    t.node == node_ix
                        && matches!(t.wait, WaitKind::Join { arrived_via } if arrived_via == flow)
                })
                .map(|(id, _)| *id)
        };

        if arrived(state, via).is_some() {
            return Err(StepError::Invariant(format!(
                "second token arrived at join '{}' via flow '{}' — the linter's block \
                 structure guarantee is broken",
                node.id,
                self.proc.flow(via).id
            )));
        }
        state.tokens.insert(
            token,
            Token {
                node: node_ix,
                wait: WaitKind::Join { arrived_via: via },
            },
        );

        let parked: Vec<TokenId> = node
            .incoming
            .iter()
            .map(|&flow| arrived(state, flow))
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default();
        if parked.len() == node.incoming.len() {
            for id in parked {
                state.tokens.remove(&id);
            }
            self.element_started(node_ix);
            self.element_completed(node_ix);
            let continuation = state.next_token_id();
            self.leave_single(state, continuation, node_ix)?;
        }
        Ok(())
    }

    /// Take the node's single outgoing flow (guaranteed single by
    /// `no-implicit-split` / pure-gateway lint rules).
    fn leave_single(
        &mut self,
        state: &mut InstanceState,
        token: TokenId,
        node_ix: NodeIx,
    ) -> Result<(), StepError> {
        let node = self.proc.node(node_ix);
        match node.outgoing.as_slice() {
            [flow] => self.leave(state, token, *flow),
            other => Err(StepError::Invariant(format!(
                "'{}' should have exactly one outgoing flow, has {}",
                node.id,
                other.len()
            ))),
        }
    }

    fn leave(
        &mut self,
        _state: &mut InstanceState,
        token: TokenId,
        flow: FlowIx,
    ) -> Result<(), StepError> {
        let f = self.proc.flow(flow);
        self.events.push(Event::FlowTaken { flow: f.id.clone() });
        self.queue.push_back((token, f.target, Some(flow)));
        Ok(())
    }

    fn element_started(&mut self, node: NodeIx) {
        self.events.push(Event::ElementStarted {
            element: self.proc.node_id(node).to_string(),
        });
    }

    fn element_completed(&mut self, node: NodeIx) {
        self.events.push(Event::ElementCompleted {
            element: self.proc.node_id(node).to_string(),
        });
    }
}
