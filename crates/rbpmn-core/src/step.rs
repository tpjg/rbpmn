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
    InstanceState, InstanceStatus, ScopeId, ScopeState, SubscriptionId, SubscriptionState, TimerId,
    TimerState, Token, TokenId, WaitKind, WorkItemId, WorkItemState,
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
    /// An armed timer became due. Time never enters the core any other way:
    /// the projection decides *when* from database time; the core only ever
    /// sees the fact. Firing a catch timer resumes its token; a boundary
    /// timer interrupts its host; an event-gateway timer wins the race.
    FireTimer { id: TimerId },
    /// A correlated message arrived for an open subscription, carrying an
    /// RFC 7386 merge patch (like work-item completion).
    DeliverMessage { id: SubscriptionId, patch: Value },
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
    /// Fired, cancelled and never-armed timers are indistinguishable here:
    /// armed timers are the only ones that exist. The projection's row claim
    /// guarantees a timer is fired at most once.
    #[error("no armed timer {0:?} in this instance")]
    UnknownTimer(TimerId),
    #[error("no open subscription {0:?} in this instance")]
    UnknownSubscription(SubscriptionId),
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
            adv.queue.push_back(Move {
                token,
                node: proc.start(),
                via: None,
                scope: ScopeId::ROOT,
            });
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
            let Some(parked) = state.tokens.remove(&token_id) else {
                return Err(StepError::Invariant(format!(
                    "work item {id:?} referenced token {token_id:?} which does not exist"
                )));
            };

            let mut adv = Advancer::new(proc);
            adv.scope = parked.scope;
            adv.events.push(Event::WorkItemCompleted {
                id,
                element: proc.node_id(element).to_string(),
            });
            if patch != Value::Object(serde_json::Map::new()) {
                merge_patch(&mut state.variables, &patch);
                adv.events.push(Event::VariablesPatched { patch });
            }
            // The host completed: its interrupting boundary timers disarm.
            adv.cancel_attachments(state, token_id);
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

            // An error is caught by a boundary on the failing task, or —
            // failing that — by one on the nearest enclosing subprocess:
            // the scoped error handler. Each step outward interrupts that
            // subprocess's token, tearing its whole scope down.
            let mut caught = None;
            if let Some(c) = code.as_deref() {
                let mut host = element;
                let mut target = token_id;
                let mut scope = state
                    .tokens
                    .get(&token_id)
                    .map(|t| t.scope)
                    .unwrap_or(ScopeId::ROOT);
                loop {
                    if let Some(boundary) = proc.error_boundary(host, c) {
                        caught = Some((target, boundary));
                        break;
                    }
                    let Some(enclosing) = state.scopes.get(&scope) else {
                        break; // reached the instance root uncaught
                    };
                    host = enclosing.element;
                    target = enclosing.token;
                    scope = enclosing.parent;
                }
            }
            match caught {
                Some((target, boundary_ix)) => {
                    // When the catcher is an enclosing subprocess, the
                    // failing task's token is inside the doomed scope, so
                    // the teardown reaps it — *with* its armed boundary
                    // timers. Removing it here first would hide it from
                    // `tear_down_scope`, stranding those timers on a token
                    // that no longer exists: the scheduler would later fire
                    // one and wedge the instance on an Invariant error.
                    adv.interrupt_to_boundary(state, target, boundary_ix)?;
                    adv.run(state)
                }
                None => {
                    // Incident: the uniform freeze (token parked at the
                    // failed task, boundary timers withdrawn, instance
                    // frozen for repair).
                    adv.freeze(state, token_id, element, code);
                    Ok(adv.events)
                }
            }
        }
        Command::FireTimer { id } => {
            if state.status != InstanceStatus::Active {
                return Err(StepError::InstanceNotActive(state.status));
            }
            let timer = state
                .timers
                .remove(&id)
                .ok_or(StepError::UnknownTimer(id))?;
            let mut adv = Advancer::new(proc);
            adv.events.push(Event::TimerFired {
                id,
                element: proc.node_id(timer.element).to_string(),
            });
            let token = state.tokens.get(&timer.token).cloned().ok_or_else(|| {
                StepError::Invariant(format!(
                    "timer {id:?} referenced token {:?} which does not exist",
                    timer.token
                ))
            })?;
            match token.wait {
                // The token sits at the timer catch itself: resume it.
                WaitKind::Timer(tid) if tid == id => {
                    adv.scope = token.scope;
                    state.tokens.remove(&timer.token);
                    adv.element_completed(timer.element);
                    adv.leave_single(state, timer.token, timer.element)?;
                    adv.run(state)
                }
                // Interrupting boundary on a subprocess: the timer kills the
                // whole scope, recursively, and the boundary path is taken.
                WaitKind::Scope(_) => {
                    adv.interrupt_to_boundary(state, timer.token, timer.element)?;
                    adv.run(state)
                }
                // Interrupting boundary on a task: cancel the work item and
                // continue on the boundary path — the host never completes.
                WaitKind::WorkItem(wid) => {
                    let item = state.work_items.get_mut(&wid).ok_or_else(|| {
                        StepError::Invariant(format!(
                            "boundary timer {id:?} host work item {wid:?} does not exist"
                        ))
                    })?;
                    item.open = false;
                    let host_element = item.element;
                    adv.events.push(Event::WorkItemCancelled {
                        id: wid,
                        element: proc.node_id(host_element).to_string(),
                    });
                    adv.interrupt_to_boundary(state, timer.token, timer.element)?;
                    adv.run(state)
                }
                // Interrupting boundary on a receive task: the open
                // subscription is withdrawn instead of a work item.
                WaitKind::Message(sid) => {
                    let sub = state.subscriptions.remove(&sid).ok_or_else(|| {
                        StepError::Invariant(format!(
                            "boundary timer {id:?} host subscription {sid:?} does not exist"
                        ))
                    })?;
                    adv.events.push(Event::SubscriptionCancelled {
                        id: sid,
                        element: proc.node_id(sub.element).to_string(),
                        message: sub.message,
                    });
                    adv.interrupt_to_boundary(state, timer.token, timer.element)?;
                    adv.run(state)
                }
                // The race at an event-based gateway: this timer won, every
                // other armed event on the token is withdrawn.
                WaitKind::EventGateway => {
                    adv.scope = token.scope;
                    adv.cancel_attachments(state, timer.token);
                    adv.take_gateway_path(state, timer.token, token.node, timer.element)?;
                    adv.run(state)
                }
                WaitKind::Timer(_) | WaitKind::Join { .. } | WaitKind::Incident => {
                    Err(StepError::Invariant(format!(
                        "timer {id:?} fired on a token in an unrelated wait state"
                    )))
                }
            }
        }
        Command::DeliverMessage { id, patch } => {
            if state.status != InstanceStatus::Active {
                return Err(StepError::InstanceNotActive(state.status));
            }
            let sub = state
                .subscriptions
                .remove(&id)
                .ok_or(StepError::UnknownSubscription(id))?;
            let mut adv = Advancer::new(proc);
            adv.events.push(Event::MessageReceived {
                id,
                element: proc.node_id(sub.element).to_string(),
                message: sub.message.clone(),
            });
            if patch != Value::Object(serde_json::Map::new()) {
                merge_patch(&mut state.variables, &patch);
                adv.events.push(Event::VariablesPatched { patch });
            }
            let token = state.tokens.get(&sub.token).cloned().ok_or_else(|| {
                StepError::Invariant(format!(
                    "subscription {id:?} referenced token {:?} which does not exist",
                    sub.token
                ))
            })?;
            match token.wait {
                // The token sits at the catch (or receive task): resume it,
                // disarming any boundary timers on it.
                WaitKind::Message(sid) if sid == id => {
                    adv.scope = token.scope;
                    adv.cancel_attachments(state, sub.token);
                    state.tokens.remove(&sub.token);
                    adv.element_completed(sub.element);
                    adv.leave_single(state, sub.token, sub.element)?;
                    adv.run(state)
                }
                // The race at an event-based gateway: this message won.
                WaitKind::EventGateway => {
                    adv.scope = token.scope;
                    adv.cancel_attachments(state, sub.token);
                    adv.take_gateway_path(state, sub.token, token.node, sub.element)?;
                    adv.run(state)
                }
                _ => Err(StepError::Invariant(format!(
                    "message {id:?} delivered to a token in an unrelated wait state"
                ))),
            }
        }
    }
}

/// A token in flight: not yet parked, so it lives in the queue rather than
/// in the state.
#[derive(Debug, Clone, Copy)]
struct Move {
    token: TokenId,
    node: NodeIx,
    via: Option<FlowIx>,
    scope: ScopeId,
}

struct Advancer<'a> {
    proc: &'a ExecutableProcess,
    events: Vec<Event>,
    queue: VecDeque<Move>,
    /// The runtime scope of the move being processed. Sequence flows never
    /// cross a scope boundary (`bpmn-structure` rejects endpoints that do
    /// not resolve within one scope — see the `cross-scope-flow`
    /// fixture), so a token
    /// following a flow stays in this scope — which is why parking and
    /// leaving can read it here instead of threading it through every
    /// signature. The two places that *do* change scope — entering a
    /// subprocess and resuming its parent — set it explicitly.
    scope: ScopeId,
}

impl<'a> Advancer<'a> {
    fn new(proc: &'a ExecutableProcess) -> Self {
        Advancer {
            proc,
            events: Vec::new(),
            queue: VecDeque::new(),
            scope: ScopeId::ROOT,
        }
    }

    fn run(mut self, state: &mut InstanceState) -> Result<Vec<Event>, StepError> {
        while let Some(mv) = self.queue.pop_front() {
            if state.status != InstanceStatus::Active {
                break;
            }
            self.scope = mv.scope;
            self.enter(state, mv.token, mv.node, mv.via)?;
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
                        scope: self.scope,
                        wait: WaitKind::WorkItem(item),
                    },
                );
                self.events.push(Event::WorkItemCreated {
                    id: item,
                    element: node.id.clone(),
                    work_kind: *kind,
                    topic: topic.clone(),
                });
                self.arm_boundaries(state, token, node_ix);
                Ok(())
            }
            ExecKind::TimerCatch { due } => {
                self.element_started(node_ix);
                let id = self.arm_timer(state, token, node_ix, due.clone());
                state.tokens.insert(
                    token,
                    Token {
                        node: node_ix,
                        scope: self.scope,
                        wait: WaitKind::Timer(id),
                    },
                );
                Ok(())
            }
            ExecKind::MessageCatch { .. } => {
                self.element_started(node_ix);
                let Some(id) = self.subscribe(state, token, node_ix) else {
                    return Ok(()); // correlation incident: frozen for repair
                };
                state.tokens.insert(
                    token,
                    Token {
                        node: node_ix,
                        scope: self.scope,
                        wait: WaitKind::Message(id),
                    },
                );
                // A receive task can carry interrupting timer boundaries.
                self.arm_boundaries(state, token, node_ix);
                Ok(())
            }
            ExecKind::EventBasedGateway => {
                self.element_started(node_ix);
                state.tokens.insert(
                    token,
                    Token {
                        node: node_ix,
                        scope: self.scope,
                        wait: WaitKind::EventGateway,
                    },
                );
                // Arm every alternative on this token, in declaration order;
                // they race, and the winner withdraws the rest.
                for flow in self.proc.node(node_ix).outgoing.clone() {
                    let target = self.proc.flow(flow).target;
                    match &self.proc.node(target).kind {
                        ExecKind::TimerCatch { due } => {
                            self.arm_timer(state, token, target, due.clone());
                        }
                        ExecKind::MessageCatch { .. } => {
                            if self.subscribe(state, token, target).is_none() {
                                return Ok(()); // correlation incident
                            }
                        }
                        other => {
                            return Err(StepError::Invariant(format!(
                                "event gateway '{}' targets {:?} — lint should \
                                 have prevented this",
                                node.id, other
                            )));
                        }
                    }
                }
                Ok(())
            }
            ExecKind::SubProcess { scope } => {
                let child_static = *scope;
                self.element_started(node_ix);
                // The parent token parks here; a fresh runtime scope opens
                // and a token starts inside it. Entering twice (a loop)
                // opens a *new* scope each time, which is what keeps two
                // iterations' joins and teardowns from seeing each other.
                let child = state.alloc_scope(ScopeState {
                    element: node_ix,
                    parent: self.scope,
                    token,
                });
                state.tokens.insert(
                    token,
                    Token {
                        node: node_ix,
                        scope: self.scope,
                        wait: WaitKind::Scope(child),
                    },
                );
                // Boundary timers on the subprocess arm on the parent token,
                // exactly as they do for a task.
                self.arm_boundaries(state, token, node_ix);
                let inner = state.next_token_id();
                self.queue.push_back(Move {
                    token: inner,
                    node: self.proc.scope_start(child_static),
                    via: None,
                    scope: child,
                });
                Ok(())
            }
            ExecKind::TimerBoundary { .. } => Err(StepError::Invariant(format!(
                "timer boundary '{}' entered via a sequence flow",
                node.id
            ))),
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
                // Token is consumed: it was never parked, simply not
                // re-queued. If it was the last one in a subprocess scope,
                // that scope is finished and its parent resumes.
                self.complete_scope_if_empty(state, self.scope)
            }
            ExecKind::TerminateEnd => {
                self.element_started(node_ix);
                self.element_completed(node_ix);
                if self.scope != ScopeId::ROOT {
                    // Scope-local (BPMN 2.0): a terminate inside a subprocess
                    // ends *that subprocess*. Its siblings inside the scope
                    // are torn down, then the parent token leaves the
                    // subprocess normally — the instance keeps running.
                    let scope = self.scope;
                    self.tear_down_scope(state, scope);
                    return self.complete_scope(state, scope);
                }
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
                // Everything of the instance goes in one transaction:
                // tokens, work items, timers, subscriptions, scopes.
                self.withdraw_arms(state, None);
                state.tokens.clear();
                state.scopes.clear();
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

        // Scope-local counting: a join waits for one token per incoming flow
        // *within its own scope instance*, so two iterations of a subprocess
        // (or two sibling scopes) never satisfy each other's joins.
        let scope = self.scope;
        let arrived = |state: &InstanceState, flow: FlowIx| {
            state
                .tokens
                .iter()
                .find(|(_, t)| {
                    t.node == node_ix
                        && t.scope == scope
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
                scope: self.scope,
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
        self.queue.push_back(Move {
            token,
            node: f.target,
            via: Some(flow),
            scope: self.scope,
        });
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

    /// Arm the host's interrupting timer boundaries on its parked token.
    fn arm_boundaries(&mut self, state: &mut InstanceState, token: TokenId, host: NodeIx) {
        for b in self.proc.timer_boundaries(host).to_vec() {
            let ExecKind::TimerBoundary { due } = &self.proc.node(b).kind else {
                unreachable!("timer_boundaries only holds timer boundary nodes");
            };
            let due = due.clone();
            self.arm_timer(state, token, b, due);
        }
    }

    /// The single timer-arming chokepoint (mirror of `subscribe`): allocate,
    /// record, emit — every armed timer goes through here.
    fn arm_timer(
        &mut self,
        state: &mut InstanceState,
        token: TokenId,
        element: NodeIx,
        due: crate::compile::TimerDue,
    ) -> TimerId {
        let id = state.alloc_timer(TimerState {
            element,
            token,
            due: due.clone(),
        });
        self.events.push(Event::TimerArmed {
            id,
            element: self.proc.node_id(element).to_string(),
            due,
            token,
        });
        id
    }

    /// Interrupting boundary taken: the host's token leaves on the boundary
    /// path, its remaining arms withdrawn first. The host never completes.
    fn interrupt_to_boundary(
        &mut self,
        state: &mut InstanceState,
        token: TokenId,
        boundary: NodeIx,
    ) -> Result<(), StepError> {
        let Some(parked) = state.tokens.remove(&token) else {
            return Err(StepError::Invariant(format!(
                "token {token:?} vanished before its boundary interrupt"
            )));
        };
        // Interrupting a subprocess kills everything inside it, recursively.
        if let WaitKind::Scope(child) = parked.wait {
            self.tear_down_scope(state, child);
            state.scopes.remove(&child);
        }
        // The boundary path continues in the host's own scope.
        self.scope = parked.scope;
        self.cancel_attachments(state, token);
        self.element_started(boundary);
        self.element_completed(boundary);
        self.leave_single(state, token, boundary)
    }

    /// Open a subscription for the message catch at `element`, evaluating
    /// its correlation key from the variables **now** (arm time). Keys must
    /// be strings or exact integers (floats have no canonical spelling
    /// across a jsonb round-trip — the same logical value would arm two
    /// different keys); anything else can never match. Both cases, and a
    /// duplicate open (message, key) in this instance (which would make
    /// every delivery permanently ambiguous), freeze the instance as an
    /// incident instead of waiting forever.
    fn subscribe(
        &mut self,
        state: &mut InstanceState,
        token: TokenId,
        element: NodeIx,
    ) -> Option<SubscriptionId> {
        let ExecKind::MessageCatch { message, key } = &self.proc.node(element).kind else {
            unreachable!("subscribe is only called on message catch nodes");
        };
        let value = rbpmn_model::condition::resolve_path(&state.variables, key);
        let key_value = match value {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => n
                .as_i64()
                .map(|i| i.to_string())
                .or_else(|| n.as_u64().map(|u| u.to_string())),
            _ => None,
        };
        let Some(key_value) = key_value else {
            self.events.push(Event::CorrelationFailed {
                element: self.proc.node_id(element).to_string(),
                name: key.join("."),
            });
            self.freeze(state, token, element, None);
            return None;
        };
        if state
            .subscriptions
            .values()
            .any(|s| s.message == *message && s.key == key_value)
        {
            self.events.push(Event::DuplicateSubscription {
                element: self.proc.node_id(element).to_string(),
                message: message.clone(),
                key: key_value,
            });
            self.freeze(state, token, element, None);
            return None;
        }
        let id = state.alloc_subscription(SubscriptionState {
            element,
            token,
            message: message.clone(),
            key: key_value.clone(),
        });
        self.events.push(Event::MessageSubscribed {
            id,
            element: self.proc.node_id(element).to_string(),
            message: message.clone(),
            key: key_value,
            token,
        });
        Some(id)
    }

    /// Every incident converges here: withdraw the token's in-flight arms,
    /// park it at the failing element (`WaitKind::Incident` — inspection
    /// shows *where*, and a future repair API has one shape to resume), and
    /// freeze the instance. Tokens still queued in this advancement (a
    /// parallel sibling mid-transit) park at their target elements the same
    /// way — frozen means *nothing advances and nothing vanishes*; token
    /// conservation must survive the freeze or no repair can ever resume.
    /// The cause event is pushed by the caller first; `incident-raised`
    /// closes the sequence.
    fn freeze(
        &mut self,
        state: &mut InstanceState,
        token: TokenId,
        element: NodeIx,
        code: Option<String>,
    ) {
        self.cancel_attachments(state, token);
        let scope = state
            .tokens
            .get(&token)
            .map(|t| t.scope)
            .unwrap_or(self.scope);
        state.tokens.insert(
            token,
            Token {
                node: element,
                scope,
                wait: WaitKind::Incident,
            },
        );
        for mv in std::mem::take(&mut self.queue) {
            state.tokens.insert(
                mv.token,
                Token {
                    node: mv.node,
                    scope: mv.scope,
                    wait: WaitKind::Incident,
                },
            );
        }
        state.status = InstanceStatus::Failed;
        self.events.push(Event::IncidentRaised {
            element: self.proc.node_id(element).to_string(),
            code,
        });
    }

    /// A scope finishes when its last token is consumed. Completing it
    /// emits the subprocess's `element-completed`, withdraws the boundary
    /// timers armed on the parent token, and resumes that token on the
    /// subprocess's outgoing flow — in the *parent* scope.
    fn complete_scope_if_empty(
        &mut self,
        state: &mut InstanceState,
        scope: ScopeId,
    ) -> Result<(), StepError> {
        if scope == ScopeId::ROOT || !self.scope_is_empty(state, scope) {
            return Ok(());
        }
        self.complete_scope(state, scope)
    }

    /// No token of `scope` remains — neither parked nor still in flight.
    /// The queue matters: a sibling branch mid-advance is not "gone".
    fn scope_is_empty(&self, state: &InstanceState, scope: ScopeId) -> bool {
        // A nested scope still open needs no separate check: its parent
        // token is parked in this scope (`WaitKind::Scope`), so the token
        // test below already reports the scope as non-empty.
        !state.tokens.values().any(|t| t.scope == scope)
            && !self.queue.iter().any(|m| m.scope == scope)
    }

    fn complete_scope(
        &mut self,
        state: &mut InstanceState,
        scope: ScopeId,
    ) -> Result<(), StepError> {
        let Some(closed) = state.scopes.remove(&scope) else {
            return Err(StepError::Invariant(format!(
                "scope {scope:?} completed twice"
            )));
        };
        // Cancellation before completion, matching the task path's order
        // (work-item-completed, timer-cancelled, element-completed).
        self.cancel_attachments(state, closed.token);
        self.element_completed(closed.element);
        if state.tokens.remove(&closed.token).is_none() {
            return Err(StepError::Invariant(format!(
                "scope {scope:?} had no parked parent token"
            )));
        }
        // The parent continues in ITS scope, not the one that just closed.
        self.scope = closed.parent;
        self.leave_single(state, closed.token, closed.element)
    }

    /// Cancel everything inside `scope` and its nested scopes: queued moves,
    /// parked tokens, open work items, armed timers and subscriptions. The
    /// scope entry itself survives for the caller to complete or discard.
    fn tear_down_scope(&mut self, state: &mut InstanceState, scope: ScopeId) {
        let doomed = state.scope_subtree(scope);
        self.queue.retain(|m| !doomed.contains(&m.scope));
        let tokens: Vec<TokenId> = state
            .tokens
            .iter()
            .filter(|(_, t)| doomed.contains(&t.scope))
            .map(|(id, _)| *id)
            .collect();
        for token in &tokens {
            let open: Vec<WorkItemId> = state
                .work_items
                .iter()
                .filter(|(_, w)| w.open && w.token == *token)
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
            self.withdraw_arms(state, Some(*token));
            state.tokens.remove(token);
        }
        // Nested scopes are gone with their tokens; `scope` itself is the
        // caller's to close.
        state
            .scopes
            .retain(|id, _| *id == scope || !doomed.contains(id));
    }

    /// Withdraw every remaining timer/subscription attached to `token` —
    /// boundary timers when their host resolves, or the losing alternatives
    /// of an event-based gateway.
    fn cancel_attachments(&mut self, state: &mut InstanceState, token: TokenId) {
        self.withdraw_arms(state, Some(token));
    }

    /// Withdraw armed timers and open subscriptions — one token's (boundary
    /// disarm, gateway race) or every token's (terminate). Timers first,
    /// then subscriptions, each in id order: the deterministic cancellation
    /// order the golden traces pin.
    fn withdraw_arms(&mut self, state: &mut InstanceState, token: Option<TokenId>) {
        let timers: Vec<TimerId> = state
            .timers
            .iter()
            .filter(|(_, t)| token.is_none_or(|tok| t.token == tok))
            .map(|(id, _)| *id)
            .collect();
        for id in timers {
            let timer = state.timers.remove(&id).unwrap();
            self.events.push(Event::TimerCancelled {
                id,
                element: self.proc.node_id(timer.element).to_string(),
            });
        }
        let subs: Vec<SubscriptionId> = state
            .subscriptions
            .iter()
            .filter(|(_, s)| token.is_none_or(|tok| s.token == tok))
            .map(|(id, _)| *id)
            .collect();
        for id in subs {
            let sub = state.subscriptions.remove(&id).unwrap();
            self.events.push(Event::SubscriptionCancelled {
                id,
                element: self.proc.node_id(sub.element).to_string(),
                message: sub.message,
            });
        }
    }

    /// An event-based gateway resolved: complete the gateway, walk the flow
    /// to the winning catch element and past it. The catch is *not* entered
    /// via the queue — entering would re-arm it; its wait already happened.
    fn take_gateway_path(
        &mut self,
        state: &mut InstanceState,
        token: TokenId,
        gateway: NodeIx,
        winner: NodeIx,
    ) -> Result<(), StepError> {
        state.tokens.remove(&token);
        self.element_completed(gateway);
        let flow = self
            .proc
            .node(gateway)
            .outgoing
            .iter()
            .copied()
            .find(|f| self.proc.flow(*f).target == winner)
            .ok_or_else(|| {
                StepError::Invariant(format!(
                    "no flow from gateway '{}' to winning event '{}'",
                    self.proc.node_id(gateway),
                    self.proc.node_id(winner)
                ))
            })?;
        self.events.push(Event::FlowTaken {
            flow: self.proc.flow(flow).id.clone(),
        });
        self.element_started(winner);
        self.element_completed(winner);
        self.leave_single(state, token, winner)
    }
}
