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
    InstanceState, InstanceStatus, SubscriptionId, SubscriptionState, TimerId, TimerState, Token,
    TokenId, WaitKind, WorkItemId, WorkItemState,
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

            let boundary = code
                .as_deref()
                .and_then(|c| proc.error_boundary(element, c));
            match boundary {
                Some(boundary_ix) => {
                    // Interrupting: the task's token continues on the
                    // boundary path; sibling boundary timers disarm.
                    if state.tokens.remove(&token_id).is_none() {
                        return Err(StepError::Invariant(format!(
                            "work item {id:?} referenced token {token_id:?} which does not exist"
                        )));
                    }
                    adv.cancel_attachments(state, token_id);
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
                    state.tokens.remove(&timer.token);
                    adv.element_completed(timer.element);
                    adv.leave_single(state, timer.token, timer.element)?;
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
                    adv.cancel_attachments(state, timer.token);
                    state.tokens.remove(&timer.token);
                    adv.element_started(timer.element);
                    adv.element_completed(timer.element);
                    adv.leave_single(state, timer.token, timer.element)?;
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
                    adv.cancel_attachments(state, timer.token);
                    state.tokens.remove(&timer.token);
                    adv.element_started(timer.element);
                    adv.element_completed(timer.element);
                    adv.leave_single(state, timer.token, timer.element)?;
                    adv.run(state)
                }
                // The race at an event-based gateway: this timer won, every
                // other armed event on the token is withdrawn.
                WaitKind::EventGateway => {
                    adv.cancel_attachments(state, timer.token);
                    adv.take_gateway_path(state, timer.token, token.node, timer.element)?;
                    adv.run(state)
                }
                WaitKind::Timer(_) | WaitKind::Join { .. } => Err(StepError::Invariant(format!(
                    "timer {id:?} fired on a token in an unrelated wait state"
                ))),
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
                    adv.cancel_attachments(state, sub.token);
                    state.tokens.remove(&sub.token);
                    adv.element_completed(sub.element);
                    adv.leave_single(state, sub.token, sub.element)?;
                    adv.run(state)
                }
                // The race at an event-based gateway: this message won.
                WaitKind::EventGateway => {
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
                self.arm_boundaries(state, token, node_ix);
                Ok(())
            }
            ExecKind::TimerCatch { due } => {
                self.element_started(node_ix);
                let due = due.clone();
                let id = state.alloc_timer(TimerState {
                    element: node_ix,
                    token,
                    due: due.clone(),
                });
                state.tokens.insert(
                    token,
                    Token {
                        node: node_ix,
                        wait: WaitKind::Timer(id),
                    },
                );
                self.events.push(Event::TimerArmed {
                    id,
                    element: node.id.clone(),
                    due,
                    token,
                });
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
                        wait: WaitKind::EventGateway,
                    },
                );
                // Arm every alternative on this token, in declaration order;
                // they race, and the winner withdraws the rest.
                for flow in self.proc.node(node_ix).outgoing.clone() {
                    let target = self.proc.flow(flow).target;
                    match &self.proc.node(target).kind {
                        ExecKind::TimerCatch { due } => {
                            let due = due.clone();
                            let id = state.alloc_timer(TimerState {
                                element: target,
                                token,
                                due: due.clone(),
                            });
                            self.events.push(Event::TimerArmed {
                                id,
                                element: self.proc.node_id(target).to_string(),
                                due,
                                token,
                            });
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
                // Everything of the instance goes in one transaction:
                // tokens, work items, timers, subscriptions.
                let timers: Vec<TimerId> = state.timers.keys().copied().collect();
                for id in timers {
                    let timer = state.timers.remove(&id).unwrap();
                    self.events.push(Event::TimerCancelled {
                        id,
                        element: self.proc.node_id(timer.element).to_string(),
                    });
                }
                let subs: Vec<SubscriptionId> = state.subscriptions.keys().copied().collect();
                for id in subs {
                    let sub = state.subscriptions.remove(&id).unwrap();
                    self.events.push(Event::SubscriptionCancelled {
                        id,
                        element: self.proc.node_id(sub.element).to_string(),
                        message: sub.message,
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

    /// Arm the host's interrupting timer boundaries on its parked token.
    fn arm_boundaries(&mut self, state: &mut InstanceState, token: TokenId, host: NodeIx) {
        for b in self.proc.timer_boundaries(host).to_vec() {
            let ExecKind::TimerBoundary { due } = &self.proc.node(b).kind else {
                unreachable!("timer_boundaries only holds timer boundary nodes");
            };
            let due = due.clone();
            let id = state.alloc_timer(TimerState {
                element: b,
                token,
                due: due.clone(),
            });
            self.events.push(Event::TimerArmed {
                id,
                element: self.proc.node_id(b).to_string(),
                due,
                token,
            });
        }
    }

    /// Open a subscription for the message catch at `element`, evaluating
    /// its correlation key from the variables **now** (arm time). A key that
    /// is not a string or number can never match — the instance freezes as
    /// an incident instead of waiting forever.
    fn subscribe(
        &mut self,
        state: &mut InstanceState,
        token: TokenId,
        element: NodeIx,
    ) -> Option<SubscriptionId> {
        let ExecKind::MessageCatch {
            message,
            key,
            key_name,
        } = &self.proc.node(element).kind
        else {
            unreachable!("subscribe is only called on message catch nodes");
        };
        let value = rbpmn_model::condition::resolve_path(&state.variables, key);
        let key_value = match value {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        };
        match key_value {
            Some(key_value) => {
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
            None => {
                self.events.push(Event::CorrelationFailed {
                    element: self.proc.node_id(element).to_string(),
                    name: key_name.clone(),
                });
                self.events.push(Event::IncidentRaised {
                    element: self.proc.node_id(element).to_string(),
                    code: None,
                });
                state.status = InstanceStatus::Failed;
                None
            }
        }
    }

    /// Withdraw every remaining timer/subscription attached to `token` —
    /// boundary timers when their host resolves, or the losing alternatives
    /// of an event-based gateway.
    fn cancel_attachments(&mut self, state: &mut InstanceState, token: TokenId) {
        let timers: Vec<TimerId> = state
            .timers
            .iter()
            .filter(|(_, t)| t.token == token)
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
            .filter(|(_, s)| s.token == token)
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
