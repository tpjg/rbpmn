//! The step function — the correctness core.
//!
//! A step takes a quiescent state and one command, advances every affected
//! token synchronously to its next wait position, and returns the events.
//! One wait position is not a resting place: [`WaitKind::Decision`] is a
//! *request*, and the caller must answer it with [`Command::CompleteDecision`]
//! before the transaction ends. Persistence refuses to write one, and a freeze
//! takes any still pending along with it.
//! Deterministic by construction: tokens advance breadth-first in a FIFO
//! queue, split branches spawn in sequence-flow declaration order, and all
//! collections iterate in id order — the same inputs always produce the same
//! trace (what makes golden-log fixtures possible).
//!
//! Errors are returned before any mutation, except [`StepError::Invariant`],
//! which signals a bug in the engine (lint-clean models cannot trigger it)
//! and poisons the state.

use crate::compile::{ExecKind, ExecutableProcess, FlowIx, NodeIx, TimerDue};
use crate::event::{Event, RepairKind};
use crate::merge_patch::merge_patch;
use crate::state::{
    Halt, InstanceState, InstanceStatus, ScopeId, ScopeState, SubscriptionId, SubscriptionState,
    TimerId, TimerState, Token, TokenId, WaitKind, WorkItemId, WorkItemState,
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
    /// A work item's retry budget is exhausted: raise the error, with its
    /// code if it has one. The nearest matching error boundary — on the host,
    /// else on an enclosing subprocess, an exact code before a catch-all at
    /// each — interrupts and takes the boundary path; no match freezes the
    /// instance in the incident state.
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
    /// RFC 7386 merge patch (like work-item completion). Delivering to a
    /// catch resumes its token; to a message boundary interrupts its host
    /// (work item withdrawn, subscription withdrawn, or the whole scope torn
    /// down) and takes the boundary path; to an event-gateway alternative
    /// wins the race — the same three shapes `FireTimer` has.
    DeliverMessage { id: SubscriptionId, patch: Value },
    /// A decision was evaluated for the token parked at `token`.
    ///
    /// Time and decisions enter this core the same way: as command data. The
    /// projection evaluates inside the step's transaction and hands the
    /// answer back, so a replay reads the recorded answer instead of running
    /// an evaluator — which is what lets `chaos.rs` re-derive every history
    /// through a core that cannot evaluate anything.
    ///
    /// The three shapes an evaluator can hand back: `Some(v)` is an answer,
    /// `None` is a failure, and `reason` carries the evaluator's prose for
    /// either — a *null* answer is still an answer, and its reason is the only
    /// thing separating "no rule matched" from "the input was the wrong type".
    ///
    /// A `None` answer freezes the token at the element as an incident, and
    /// that incident is **not catchable by an error boundary** — see the
    /// reasoning at the handler below. (This said the opposite until the
    /// contradiction was caught: a caller who modelled a boundary on a
    /// business-rule task would have got a frozen instance instead.)
    CompleteDecision {
        token: TokenId,
        answer: Option<Value>,
        reason: Option<String>,
    },
    /// Repair the instance's open incident (docs/design/incident-scope.md,
    /// D4–D5). Never a state edit: the disposition acts at the incident's
    /// resume point through the advancer every command uses, so every state
    /// it reaches is one a normal execution could have, and a history with
    /// repairs in it replays through this core. `incident` is the number the
    /// request names (D9); any other is refused, so a resent or stale request
    /// is answered rather than stepped.
    Repair {
        incident: u64,
        disposition: Disposition,
        reason: String,
    },
}

/// What a repair does at its incident's resume point
/// (docs/design/incident-scope.md, D5).
#[derive(Debug, Clone, PartialEq)]
pub enum Disposition {
    /// Merge the patch, then enter the resume point as if for the first
    /// time: a new work item with the manifest's budget, the decision asked
    /// again, arms resolved against the patched document.
    Retry { patch: Value },
    /// Complete the resume point and leave along its outgoing flow: a task,
    /// catch or subprocess with `patch` merged; a business-rule task with the
    /// `answer` its decision could not give, written by replacement, and no
    /// patch.
    Advance { patch: Value, answer: Option<Value> },
    /// Raise the error at the resume point and walk outward as `RaiseError`
    /// does; `None` is caught by a catch-all only.
    Divert { code: Option<String> },
    /// Consume the token as though it reached an end event.
    Abandon,
    /// End the instance as `Terminated`.
    AbandonInstance,
}

impl Disposition {
    /// Which disposition this is, as `incident-repaired` records it.
    pub fn kind(&self) -> RepairKind {
        match self {
            Disposition::Retry { .. } => RepairKind::Retry,
            Disposition::Advance { .. } => RepairKind::Advance,
            Disposition::Divert { .. } => RepairKind::Divert,
            Disposition::Abandon => RepairKind::Abandon,
            Disposition::AbandonInstance => RepairKind::AbandonInstance,
        }
    }
}

/// Why a repair was refused, before anything changed (D5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("a repair needs a reason: it is what makes the history an audit trail")]
    NoReason,
    #[error(
        "the instance froze before repair existed, with several tokens at an incident, so its \
         cause cannot be told from its collateral — abandon the instance instead"
    )]
    CauseUnknown,
    #[error("an event-based gateway has no single way on — retry it, or divert")]
    GatewayHasNoSingleWayOn,
    #[error(
        "a business-rule task advances with the answer its decision could not give, and no \
         patch"
    )]
    DecisionNeedsAnAnswer,
    #[error("only a business-rule task takes an answer — advance anything else with a patch")]
    AnswerOnlyForADecision,
    #[error(
        "no boundary catches it here or further out, and diverting into nothing would lose \
         the token"
    )]
    NothingCatches,
    #[error(
        "a join could wait for this token forever — abandon only on a side path, or the last \
         token of its scope"
    )]
    AJoinWouldWait,
}

fn open_incident_text(open: &Option<u64>) -> String {
    match open {
        Some(n) => format!("the open incident is {n}"),
        None => "the instance is not frozen".to_string(),
    }
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
    /// A delivery refused before anything changed: the non-interrupting
    /// boundary it triggers could not re-arm afterwards
    /// (docs/design/incident-scope.md, D11).
    #[error("delivery refused: boundary '{element}' could not re-arm — {reason}")]
    BoundaryCannotRearm { element: String, reason: String },
    /// A repair named an incident that is not the open one: resent, stale,
    /// or for an instance no longer frozen (docs/design/incident-scope.md,
    /// D9).
    #[error("incident {named} is not open — {}", open_incident_text(.open))]
    IncidentNotOpen { named: u64, open: Option<u64> },
    /// A repair the incident's resume point does not allow (D5).
    #[error("repair refused: {0}")]
    RepairRefused(Refusal),
    #[error("internal invariant violated: {0} — state is poisoned")]
    Invariant(String),
}

/// Write `value` at `path`, replacing whatever was there and creating the
/// objects along the way.
///
/// A non-object standing where the path needs to descend is replaced: the
/// binding says where the answer goes, and the alternative — failing the step
/// because a variable of the same name happened to be a string — would freeze
/// an instance over a naming collision.
fn assign(document: &mut Value, path: &[String], value: Value) {
    let Some((last, parents)) = path.split_last() else {
        *document = value;
        return;
    };
    let mut current = document;
    if !current.is_object() {
        *current = Value::Object(serde_json::Map::new());
    }
    for segment in parents {
        let map = current.as_object_mut().expect("made an object above");
        let entry = map
            .entry(segment.clone())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(serde_json::Map::new());
        }
        current = entry;
    }
    current
        .as_object_mut()
        .expect("made an object above")
        .insert(last.clone(), value);
}

/// Why a message arm could not be opened.
enum ArmFailure {
    /// The key did not resolve to a string or an exact integer.
    Unusable { name: String },
    /// Another open subscription already waits on this `(message, key)`.
    Duplicate { message: String, key: String },
}

impl ArmFailure {
    fn describe(&self) -> String {
        match self {
            ArmFailure::Unusable { name } => {
                format!("its correlation key '{name}' would not be a string or an exact integer")
            }
            ArmFailure::Duplicate { message, key } => format!(
                "a subscription for ({message}, {key}) is already open, and a second would \
                 make every delivery ambiguous"
            ),
        }
    }
}

/// The `(message, key)` the message arm at `element` — a catch, a receive
/// task or a message boundary — opens with against `variables`, or why it
/// cannot open. Keys must be strings or exact integers (floats have no
/// canonical spelling across a jsonb round-trip, so the same logical value
/// would arm two different keys), and a second open subscription for one
/// `(message, key)` would make every delivery permanently ambiguous.
/// `excluding` is a subscription about to be consumed, which its own re-arm
/// cannot collide with.
fn arm_key(
    proc: &ExecutableProcess,
    state: &InstanceState,
    variables: &Value,
    element: NodeIx,
    excluding: Option<SubscriptionId>,
) -> Result<(String, String), ArmFailure> {
    let Some((message, key)) = proc.message_arm(element) else {
        unreachable!("arm_key is only called on message arms");
    };
    let key_value = match rbpmn_model::condition::resolve_path(variables, key) {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => n
            .as_i64()
            .map(|i| i.to_string())
            .or_else(|| n.as_u64().map(|u| u.to_string())),
        _ => None,
    };
    let Some(key_value) = key_value else {
        return Err(ArmFailure::Unusable {
            name: key.join("."),
        });
    };
    if state
        .subscriptions
        .iter()
        .any(|(id, s)| Some(*id) != excluding && s.message == message && s.key == key_value)
    {
        return Err(ArmFailure::Duplicate {
            message: message.to_string(),
            key: key_value,
        });
    }
    Ok((message.to_string(), key_value))
}

/// The error boundary that catches `code` raised at `host`, and the token it
/// interrupts: on `host` itself, else on the nearest enclosing subprocess —
/// an exact code before a catch-all at each — whose parked token is then the
/// one interrupted, its whole scope torn down with it. `None` is uncaught.
fn catcher(
    proc: &ExecutableProcess,
    state: &InstanceState,
    host: NodeIx,
    token: TokenId,
    scope: ScopeId,
    code: Option<&str>,
) -> Option<(TokenId, NodeIx)> {
    let (mut host, mut target, mut scope) = (host, token, scope);
    loop {
        if let Some(boundary) = proc.error_boundary(host, code) {
            return Some((target, boundary));
        }
        let enclosing = state.scopes.get(&scope)?;
        host = enclosing.element;
        target = enclosing.token;
        scope = enclosing.parent;
    }
}

fn is_empty(patch: &Value) -> bool {
    *patch == Value::Object(serde_json::Map::new())
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
            // The document as it was *before* the first token moved, so the
            // history can be folded forward from here without the `Start`
            // command being kept somewhere else.
            adv.events.push(Event::InstanceStarted {
                variables: state.variables.clone(),
            });
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
        Command::CompleteDecision {
            token,
            answer,
            reason,
        } => {
            if state.status != InstanceStatus::Active {
                return Err(StepError::InstanceNotActive(state.status));
            }
            let Some(parked) = state.tokens.get(&token).cloned() else {
                return Err(StepError::Invariant(format!(
                    "decision answered for token {token:?} which does not exist"
                )));
            };
            if parked.wait != WaitKind::Decision {
                return Err(StepError::Invariant(format!(
                    "decision answered for token {token:?}, which is not waiting on one"
                )));
            }
            let element = parked.node;
            let ExecKind::BusinessRule { result, .. } = &proc.node(element).kind else {
                return Err(StepError::Invariant(format!(
                    "token {token:?} waits on a decision at a node that is not one"
                )));
            };

            let mut adv = Advancer::new(proc);
            adv.scope = parked.scope;

            let Some(answer) = answer else {
                // No usable answer: the uniform incident freeze. The token
                // parks where it failed, its in-flight arms are withdrawn,
                // and the instance freezes — so inspection shows *where*, and
                // a repair API has one state to resume from.
                //
                // Deliberately not caught by an error boundary, a catch-all
                // included. A failed decision raises no error: it has no work
                // item to fail, and is an incident of the same kind as a
                // deadline that will not resolve. Whether a catch-all *should*
                // reach it is open (`docs/design/incident-scope.md`) — that is
                // a designed contract, and a feature is never the reason one
                // ships early.
                adv.freeze(state, token, element, None, reason);
                return adv.run(state);
            };

            adv.events.push(Event::DecisionEvaluated {
                element: proc.node_id(element).to_string(),
                result: answer.clone(),
                // Only a null answer can have one, and only then is it worth
                // recording: a reason attached to a value would read as though
                // something had gone wrong with it.
                reason: reason.filter(|_| answer.is_null()),
            });
            // The answer **replaces** whatever is at the bound path. It is
            // deliberately not a merge patch, which every other write to the
            // variable document is, because RFC 7386 cannot express what a
            // decision means:
            //
            //   * `null` in a merge patch *deletes* the member, so a null
            //     answer would remove the bound path instead of storing null
            //     — and a gateway reading `result = null` would then be
            //     reading a missing value that happens to compare the same
            //     way, for a different reason;
            //   * merging an *object* answer keeps keys from the previous
            //     run, so a decision inside a loop would report a result it
            //     never produced.
            //
            // Replacement is also what a modeller means: `order.discount` is
            // the decision's answer, not an accumulation of its answers. The
            // write is recorded by `decision-evaluated`, which carries the
            // value — there is no `variables-patched` for it, because it is
            // not a patch.
            assign(&mut state.variables, result, answer);

            state.tokens.remove(&token);
            adv.cancel_attachments(state, token);
            adv.events.push(Event::ElementCompleted {
                element: proc.node_id(element).to_string(),
            });
            adv.leave_single(state, token, element)?;
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
            //
            // At each host the exact code is tried before the catch-all, and
            // the walk moves outward only when neither is there — so a nearer
            // catch-all beats a farther exact code. A failure with no code
            // can only ever meet a catch-all, and the walk runs for it too:
            // it is the shape of the failure nobody anticipated, which is
            // what a catch-all exists for.
            let scope = state
                .tokens
                .get(&token_id)
                .map(|t| t.scope)
                .unwrap_or(ScopeId::ROOT);
            match catcher(proc, state, element, token_id, scope, code.as_deref()) {
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
                    adv.freeze(state, token_id, element, code, None);
                    Ok(adv.events)
                }
            }
        }
        Command::Repair {
            incident,
            disposition,
            reason,
        } => {
            // Everything is checked before anything changes, as for every
            // refusal here: the incident named, the reason, and whether the
            // disposition is one the incident's resume point allows
            // (docs/design/incident-scope.md, D5, D9).
            let open = state.open_incident();
            if open != Some(incident) {
                return Err(StepError::IncidentNotOpen {
                    named: incident,
                    open,
                });
            }
            if reason.trim().is_empty() {
                return Err(StepError::RepairRefused(Refusal::NoReason));
            }
            // One cause per incident (D6). An instance frozen before repair
            // existed may hold several tokens at an incident and nothing to
            // tell its cause from its collateral; only abandoning it, which
            // needs no cause, is left — and its event names the first.
            let causes: Vec<TokenId> = state
                .tokens()
                .filter(|(_, t)| t.wait == WaitKind::Incident)
                .map(|(id, _)| id)
                .collect();
            let Some(&cause) = causes.first() else {
                return Err(StepError::Invariant(
                    "a frozen instance has no token at an incident".to_string(),
                ));
            };
            if causes.len() > 1 && disposition != Disposition::AbandonInstance {
                return Err(StepError::RepairRefused(Refusal::CauseUnknown));
            }
            let parked = state.tokens[&cause].clone();
            let resume = proc.resume_point(parked.node);
            let mut diverted_to = None;
            match &disposition {
                Disposition::Retry { .. } | Disposition::AbandonInstance => {}
                Disposition::Advance { patch, answer } => {
                    let decision = matches!(proc.node(resume).kind, ExecKind::BusinessRule { .. });
                    if matches!(proc.node(resume).kind, ExecKind::EventBasedGateway) {
                        return Err(StepError::RepairRefused(Refusal::GatewayHasNoSingleWayOn));
                    }
                    if decision && (answer.is_none() || !is_empty(patch)) {
                        return Err(StepError::RepairRefused(Refusal::DecisionNeedsAnAnswer));
                    }
                    if !decision && answer.is_some() {
                        return Err(StepError::RepairRefused(Refusal::AnswerOnlyForADecision));
                    }
                }
                Disposition::Divert { code } => {
                    diverted_to =
                        catcher(proc, state, resume, cause, parked.scope, code.as_deref());
                    if diverted_to.is_none() {
                        return Err(StepError::RepairRefused(Refusal::NothingCatches));
                    }
                }
                Disposition::Abandon => {
                    let last_of_its_scope = !state
                        .tokens()
                        .any(|(id, t)| id != cause && t.scope == parked.scope);
                    if !proc.on_side_path(resume) && !last_of_its_scope {
                        return Err(StepError::RepairRefused(Refusal::AJoinWouldWait));
                    }
                }
            }

            let mut adv = Advancer::new(proc);
            adv.events.push(Event::IncidentRepaired {
                incident,
                element: proc.node_id(parked.node).to_string(),
                disposition: disposition.kind(),
                code: match &disposition {
                    Disposition::Divert { code } => code.clone(),
                    _ => None,
                },
                answer: match &disposition {
                    Disposition::Advance { answer, .. } => answer.clone(),
                    _ => None,
                },
                reason,
            });
            if let Disposition::Retry { patch } | Disposition::Advance { patch, .. } = &disposition
                && !is_empty(patch)
            {
                merge_patch(&mut state.variables, patch);
                adv.events.push(Event::VariablesPatched {
                    patch: patch.clone(),
                });
            }
            // Status is derived (D6): once the cause moves nothing is parked
            // at an incident, so the instance is active before its moves run
            // and the ordinary tests apply after them — an emptied root
            // completes, a Retry that fails again freezes under a new number.
            state.status = InstanceStatus::Active;
            adv.scope = parked.scope;
            match disposition {
                Disposition::Retry { .. } => {
                    state.tokens.remove(&cause);
                    adv.queue.push_back(Move {
                        token: cause,
                        node: resume,
                        via: None,
                        scope: parked.scope,
                    });
                }
                Disposition::Advance { answer, .. } => {
                    state.tokens.remove(&cause);
                    if let (Some(answer), ExecKind::BusinessRule { result, .. }) =
                        (answer, &proc.node(resume).kind)
                    {
                        assign(&mut state.variables, result, answer);
                    }
                    adv.element_completed(resume);
                    adv.leave_single(state, cause, resume)?;
                }
                Disposition::Divert { .. } => {
                    let (target, boundary) = diverted_to.expect("checked above");
                    adv.interrupt_to_boundary(state, target, boundary)?;
                }
                Disposition::Abandon => {
                    state.tokens.remove(&cause);
                    adv.complete_scope_if_empty(state, parked.scope)?;
                }
                Disposition::AbandonInstance => {
                    adv.terminate(state);
                    return Ok(adv.events);
                }
            }
            // Collateral resumes with the cause, whatever the disposition,
            // in token order (D7): a move in flight enters its node on the
            // flow it was on; a pending decision is asked again once the
            // moves have settled.
            let halted: Vec<(TokenId, Token)> = state
                .tokens()
                .filter(|(_, t)| matches!(t.wait, WaitKind::Halted(_)))
                .map(|(id, t)| (id, t.clone()))
                .collect();
            for (id, t) in halted {
                match t.wait {
                    WaitKind::Halted(Halt::InFlight { via }) => {
                        state.tokens.remove(&id);
                        adv.queue.push_back(Move {
                            token: id,
                            node: t.node,
                            via,
                            scope: t.scope,
                        });
                    }
                    WaitKind::Halted(Halt::AwaitingDecision) => adv.reask.push(id),
                    _ => unreachable!("filtered to halted tokens"),
                }
            }
            adv.run(state)
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
                // A timer boundary fired on a waiting host. Interrupting:
                // the host is cancelled per its wait kind — work item, own
                // subscription, or the whole child scope — and the boundary
                // path is taken. One helper with `DeliverMessage`, because
                // "an arm on a parked token fired" is one thing whichever
                // kind of arm it was.
                //
                // Non-interrupting: `side_path_triggered`, the other half of
                // that pairing — the host is left exactly as it was and a
                // sibling token takes the path instead. A single-shot timer
                // does **not** re-arm — it fired once, which is what a
                // `timeDuration`/`timeDate` says, and `rearm_cycle` returns
                // having done nothing; a cycle re-arms its next occurrence
                // first, while fires remain.
                WaitKind::WorkItem(_) | WaitKind::Message(_) | WaitKind::Scope(_) => {
                    if proc.node(timer.element).kind.boundary_interrupts() {
                        adv.interrupt_host(state, timer.token, timer.element)?;
                    } else {
                        adv.side_path_triggered(
                            state,
                            timer.token,
                            timer.element,
                            |adv, state| {
                                adv.rearm_cycle(state, id, &timer);
                                true
                            },
                        )?;
                    }
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
                // `Decision` cannot be reached: it is resolved inside the
                // transaction that created it, so no timer can fire against a
                // token still holding it. A halted token never armed anything.
                WaitKind::Timer(_)
                | WaitKind::Join { .. }
                | WaitKind::Incident
                | WaitKind::Halted(_)
                | WaitKind::Decision => Err(StepError::Invariant(format!(
                    "timer {id:?} fired on a token in an unrelated wait state"
                ))),
            }
        }
        Command::DeliverMessage { id, patch } => {
            if state.status != InstanceStatus::Active {
                return Err(StepError::InstanceNotActive(state.status));
            }
            let element = state
                .subscriptions
                .get(&id)
                .ok_or(StepError::UnknownSubscription(id))?
                .element;
            // A non-interrupting boundary re-arms on this delivery, its key
            // read from the document *after* this patch. When that would
            // fail, the delivery is refused here, before anything changes:
            // the alternative is a freeze that takes the waiting host's work
            // with it (docs/design/incident-scope.md, D11).
            if matches!(
                proc.node(element).kind,
                ExecKind::MessageBoundary {
                    interrupting: false,
                    ..
                }
            ) {
                let mut patched = state.variables.clone();
                merge_patch(&mut patched, &patch);
                if let Err(failure) = arm_key(proc, state, &patched, element, Some(id)) {
                    return Err(StepError::BoundaryCannotRearm {
                        element: proc.node_id(element).to_string(),
                        reason: failure.describe(),
                    });
                }
            }
            let sub = state.subscriptions.remove(&id).expect("looked up above");
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
                // The interrupting message boundary, through the very helper
                // an interrupting timer boundary uses: cancel the host per
                // its wait kind (work item, its own subscription — this is
                // the two-subscriptions-on-one-token case — or the whole
                // child scope) and take the boundary path. The host never
                // completes, so the holder of a live lease learns at its next
                // verb (`AlreadyClosed { state: "cancelled" }`) — a lease
                // protects a worker from other workers, never from the
                // process.
                //
                // The non-interrupting one leaves the host alone and spawns a
                // sibling — and **re-arms first**, through the same
                // `side_path_triggered` a non-interrupting timer boundary
                // uses. Delivery consumed the subscription, and the boundary
                // must stay active for as long as its host is, so a new one
                // is opened immediately: a new id, and the key re-evaluated
                // against the now-patched document, because this is an arm and
                // arms evaluate at arm time. The old row is already gone, so
                // the duplicate check cannot trip on itself, and a re-arm that
                // would fail was refused above, before anything changed.
                //
                // The emission order is the one the golden traces pin:
                // `message-received`, `variables-patched`, **then** the
                // re-arm, then the side token's first move. A live host is
                // never observably without its boundary.
                WaitKind::WorkItem(_) | WaitKind::Message(_) | WaitKind::Scope(_) => {
                    if proc.node(sub.element).kind.boundary_interrupts() {
                        adv.interrupt_host(state, sub.token, sub.element)?;
                    } else {
                        adv.side_path_triggered(state, sub.token, sub.element, |adv, state| {
                            adv.subscribe(state, sub.token, sub.element).is_some()
                        })?;
                    }
                    adv.run(state)
                }
                // A timer catch hosts nothing, a join holds no arm, an
                // incident advances nothing, a halted token never armed
                // anything, and a decision never survives the transaction
                // that parked it — so none of these can own a subscription.
                WaitKind::Timer(_)
                | WaitKind::Join { .. }
                | WaitKind::Incident
                | WaitKind::Halted(_)
                | WaitKind::Decision => Err(StepError::Invariant(format!(
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
    /// Halted decisions a repair asks again once its moves have settled
    /// (docs/design/incident-scope.md, D7).
    reask: Vec<TokenId>,
}

impl<'a> Advancer<'a> {
    fn new(proc: &'a ExecutableProcess) -> Self {
        Advancer {
            proc,
            events: Vec::new(),
            queue: VecDeque::new(),
            scope: ScopeId::ROOT,
            reask: Vec::new(),
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
        // A repair's halted decisions are asked again once its moves have
        // settled — unless one of those froze the instance again, and then
        // they stay halted for the next repair.
        if state.status == InstanceStatus::Active {
            for token in std::mem::take(&mut self.reask) {
                let Some(t) = state.tokens.get_mut(&token) else {
                    continue; // reaped by a teardown one of the moves ran
                };
                t.wait = WaitKind::Decision;
                let node = t.node;
                if let ExecKind::BusinessRule { decision, .. } = &self.proc.node(node).kind {
                    self.events.push(Event::DecisionRequested {
                        element: self.proc.node_id(node).to_string(),
                        decision: decision.clone(),
                    });
                }
            }
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
            // The decision itself is not evaluated here — this crate has no
            // evaluator and must not acquire one. The token parks, the event
            // says what is needed, and the projection answers it inside the
            // same transaction (`docs/dmn.md`, D3).
            ExecKind::BusinessRule { decision, .. } => {
                self.element_started(node_ix);
                state.tokens.insert(
                    token,
                    Token {
                        node: node_ix,
                        scope: self.scope,
                        wait: WaitKind::Decision,
                    },
                );
                self.events.push(Event::DecisionRequested {
                    element: node.id.clone(),
                    decision: decision.clone(),
                });
                // No `arm_boundaries` here, and there never is one: a
                // business-rule task is not a boundary host (lint refuses
                // one, `compile` re-checks it), because this wait does not
                // survive the transaction that created it — an arm made here
                // would be withdrawn in the same step and could never fire.
                Ok(())
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
                // A boundary with an unresolvable deadline freezes here; the
                // work item is already recorded, and the freeze cancels it
                // along with everything else attached to the token.
                let _ = self.arm_boundaries(state, token, node_ix);
                Ok(())
            }
            ExecKind::TimerCatch { due } => {
                self.element_started(node_ix);
                let Some(id) = self.arm_timer(state, token, node_ix, due) else {
                    return Ok(()); // unresolvable deadline: frozen at an incident
                };
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
                let _ = self.arm_boundaries(state, token, node_ix);
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
                            if self.arm_timer(state, token, target, due).is_none() {
                                return Ok(()); // unresolvable deadline incident
                            }
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
                // exactly as they do for a task. If one cannot resolve, the
                // parent is frozen and the body must not start: entering it
                // would leave live tokens inside a scope whose owner is an
                // incident.
                if !self.arm_boundaries(state, token, node_ix) {
                    return Ok(());
                }
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
            ExecKind::MessageBoundary { .. } => Err(StepError::Invariant(format!(
                "message boundary '{}' entered via a sequence flow",
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
                self.terminate(state);
                Ok(())
            }
        }
    }

    /// End the instance as `Terminated`: every open work item cancelled,
    /// every arm withdrawn, every token and scope gone — everything of the
    /// instance in one transaction. A terminate end at the root and the
    /// abandon-instance repair both end here.
    fn terminate(&mut self, state: &mut InstanceState) {
        let open: Vec<WorkItemId> = state
            .work_items
            .iter()
            .filter(|(_, w)| w.open)
            .map(|(id, _)| *id)
            .collect();
        for id in open {
            self.cancel_work_item(state, id);
        }
        self.withdraw_arms(state, None);
        state.tokens.clear();
        state.scopes.clear();
        self.queue.clear();
        state.status = InstanceStatus::Terminated;
        self.events.push(Event::InstanceTerminated);
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

    /// Arm the host's interrupting boundaries on its parked token, in
    /// declaration order: a timer becomes an armed timer, a message an open
    /// subscription. `false` means one of them could not be armed — an
    /// unresolvable deadline, an unusable correlation key, a duplicate
    /// `(message, key)` — and the instance is frozen at an incident, so the
    /// caller must not continue; `freeze` has already withdrawn whatever this
    /// loop armed first.
    #[must_use]
    fn arm_boundaries(&mut self, state: &mut InstanceState, token: TokenId, host: NodeIx) -> bool {
        // `self.proc` outlives the advancer, so the host's list is borrowed
        // rather than copied — the loop mutates `self`, not the process.
        let proc = self.proc;
        for &b in proc.boundaries(host) {
            let armed = match &proc.node(b).kind {
                ExecKind::TimerBoundary { due, .. } => {
                    self.arm_timer(state, token, b, due).is_some()
                }
                ExecKind::MessageBoundary { .. } => self.subscribe(state, token, b).is_some(),
                other => unreachable!("boundaries holds only armable boundaries, found {other:?}"),
            };
            if !armed {
                return false;
            }
        }
        true
    }

    /// The single timer-arming chokepoint (mirror of `subscribe`): resolve,
    /// allocate, record, emit — every armed timer goes through here.
    ///
    /// A deadline read from the variable document is resolved **now**, at arm
    /// time, exactly as a correlation key is, and validated before it can
    /// reach the projection's SQL cast. `None` means it could not be: the
    /// instance is already frozen at an incident carrying the reason, and the
    /// caller must stop. Freezing rather than guessing is the whole point —
    /// firing at an invented time, or parking a token no timer will ever
    /// wake, are the two failure modes this exists to avoid.
    fn arm_timer(
        &mut self,
        state: &mut InstanceState,
        token: TokenId,
        element: NodeIx,
        source: &crate::compile::TimerSource,
    ) -> Option<TimerId> {
        let due = match source.resolve(&state.variables) {
            Ok(due) => due,
            Err(reason) => {
                self.events.push(Event::TimerResolveFailed {
                    element: self.proc.node_id(element).to_string(),
                    name: source.name(),
                    reason,
                });
                self.freeze(state, token, element, None, None);
                return None;
            }
        };
        // A cycle's repeat count is pure data the core owns; the instants
        // are the projection's. `split_cycle` cannot fail here: `resolve`
        // just validated the text with the same function.
        let remaining = match &due {
            TimerDue::Cycle(text) => rbpmn_model::iso8601::split_cycle(text)
                .ok()
                .and_then(|parts| parts.repeats),
            _ => None,
        };
        Some(self.record_timer(state, token, element, due, remaining, None))
    }

    /// Allocate a timer and say so: the tail both arming paths share, so a
    /// field added to [`TimerState`] or to `timer-armed` lands in one place
    /// rather than in one place and a half. `continues` is the only thing
    /// that differs between a first arm (`None`) and a cycle's next
    /// occurrence.
    fn record_timer(
        &mut self,
        state: &mut InstanceState,
        token: TokenId,
        element: NodeIx,
        due: TimerDue,
        remaining: Option<u32>,
        continues: Option<TimerId>,
    ) -> TimerId {
        let id = state.alloc_timer(TimerState {
            element,
            token,
            due: due.clone(),
            remaining,
        });
        self.events.push(Event::TimerArmed {
            id,
            element: self.proc.node_id(element).to_string(),
            due,
            token,
            continues,
            remaining,
        });
        id
    }

    /// A cycle fired: arm the next occurrence, unless that was the last.
    ///
    /// Emitted right after `timer-fired` and before the side token moves —
    /// the same place a message boundary re-arms, for the same reason: a
    /// live host is never observably without its boundary. `continues`
    /// carries the fired timer's id so the projection steps from *its* due,
    /// not from now; `remaining` counts down, and `Some(0)` is the end.
    fn rearm_cycle(&mut self, state: &mut InstanceState, fired: TimerId, timer: &TimerState) {
        let TimerDue::Cycle(_) = &timer.due else {
            return;
        };
        let left = timer.remaining.map(|r| r.saturating_sub(1));
        if left == Some(0) {
            return;
        }
        self.record_timer(
            state,
            timer.token,
            timer.element,
            timer.due.clone(),
            left,
            Some(fired),
        );
    }

    /// An interrupting boundary fired on a waiting host: end the host's own
    /// wait, then take the boundary path.
    ///
    /// One helper for both arms — a timer firing and a message being
    /// delivered — because what an interrupting boundary does is decided by
    /// the *host's* wait kind, never by the kind of arm that woke it. The two
    /// used to be a line-for-line copy of each other, which is one place too
    /// many for the event order below to be got right.
    ///
    /// The `subscription-cancelled` for a receive-task host is emitted
    /// **here**, before `interrupt_to_boundary` runs `cancel_attachments`:
    /// the host's own subscription goes first, then whatever else was armed
    /// on the token. That is the order the golden traces pin
    /// (`19-receive-timeout-fired.json`, `30-receive-boundary-delivered.json`).
    fn interrupt_host(
        &mut self,
        state: &mut InstanceState,
        token: TokenId,
        boundary: NodeIx,
    ) -> Result<(), StepError> {
        let wait = state
            .tokens
            .get(&token)
            .map(|t| t.wait.clone())
            .ok_or_else(|| {
                StepError::Invariant(format!(
                    "boundary '{}' host token {token:?} does not exist",
                    self.proc.node_id(boundary)
                ))
            })?;
        match wait {
            WaitKind::WorkItem(wid) => {
                if !self.cancel_work_item(state, wid) {
                    return Err(StepError::Invariant(format!(
                        "boundary '{}' host work item {wid:?} does not exist",
                        self.proc.node_id(boundary)
                    )));
                }
            }
            WaitKind::Message(sid) => {
                let host = state.subscriptions.remove(&sid).ok_or_else(|| {
                    StepError::Invariant(format!(
                        "boundary '{}' host subscription {sid:?} does not exist",
                        self.proc.node_id(boundary)
                    ))
                })?;
                self.events.push(Event::SubscriptionCancelled {
                    id: sid,
                    element: self.proc.node_id(host.element).to_string(),
                    message: host.message,
                });
            }
            // A subprocess host needs nothing here: `interrupt_to_boundary`
            // tears the child scope down recursively — work items, arms and
            // tokens at every depth — and continues in the parent scope,
            // where the boundary's flow lives.
            WaitKind::Scope(_) => {}
            other => {
                return Err(StepError::Invariant(format!(
                    "boundary '{}' fired on a host in wait state {other:?}",
                    self.proc.node_id(boundary)
                )));
            }
        }
        self.interrupt_to_boundary(state, token, boundary)
    }

    /// Close an open work item because the *process* took it away — an
    /// interrupting boundary, a terminate, a scope teardown, a freeze — and
    /// say so. The one place `work-item-cancelled` is emitted, so no
    /// withdrawal can silently forget it. `false` means there is no such
    /// item, which only a boundary interrupt can observe (the sweeping
    /// callers collected the ids they pass from the state itself).
    fn cancel_work_item(&mut self, state: &mut InstanceState, id: WorkItemId) -> bool {
        let Some(item) = state.work_items.get_mut(&id) else {
            return false;
        };
        item.open = false;
        let element = item.element;
        self.events.push(Event::WorkItemCancelled {
            id,
            element: self.proc.node_id(element).to_string(),
        });
        true
    }

    /// A non-interrupting boundary triggered on a parked host: keep its arm
    /// alive, then run the side path. The mirror of [`Self::interrupt_host`],
    /// and one helper for both arms for the same reason — what a
    /// non-interrupting boundary does is one thing whichever kind of arm woke
    /// it, and the two copies had already drifted on the scope.
    ///
    /// The order is the whole content of this function, and the golden traces
    /// pin it:
    ///
    /// 1. the **host's scope**, before anything is armed or spawned in it —
    ///    an advancer still pointing at the root would file both in the
    ///    wrong scope;
    /// 2. the **re-arm** — a fresh subscription for a message boundary, the
    ///    next occurrence for a cycle, nothing at all for a single-shot timer
    ///    (it fired once, which is what a `timeDuration` says). `false` would
    ///    mean the re-arm failed and froze the instance, leaving no side path
    ///    to run. Neither kind reaches it: a cycle re-arms from a due already
    ///    resolved, and a message re-arm that would fail was refused as a
    ///    delivery before anything changed (docs/design/incident-scope.md,
    ///    D11);
    /// 3. the **side token**. A live host is never observably without its
    ///    boundary, which is what puts the re-arm ahead of this.
    fn side_path_triggered(
        &mut self,
        state: &mut InstanceState,
        host: TokenId,
        boundary: NodeIx,
        rearm: impl FnOnce(&mut Self, &mut InstanceState) -> bool,
    ) -> Result<(), StepError> {
        self.scope = state.tokens.get(&host).map(|t| t.scope).ok_or_else(|| {
            StepError::Invariant(format!(
                "boundary '{}' host token {host:?} does not exist",
                self.proc.node_id(boundary)
            ))
        })?;
        if !rearm(self, state) {
            return Ok(()); // frozen on the re-arm
        }
        self.spawn_side_token(state, boundary)
    }

    /// The side path itself: the host's token is **untouched** — still
    /// parked, its work item / own subscription / child scope intact, its
    /// other arms still armed — and a fresh sibling token leaves along the
    /// boundary's single outgoing flow.
    ///
    /// The sibling starts in the **host token's scope**, which for a
    /// subprocess host is the *parent* scope: the boundary's flow lives
    /// beside the parked subprocess token, not inside the body.
    /// [`Self::side_path_triggered`] has already set it — the one lookup, so
    /// a re-arm and its side token cannot end up in different scopes.
    /// Nothing else is special about the sibling. It is a token: the scope
    /// completes when its last one is consumed (a host that finished first
    /// keeps the instance alive until the side work does), a teardown reaps
    /// it with everything else in its scope, and a terminate takes it. That
    /// is why scope and instance completion need no code here.
    fn spawn_side_token(
        &mut self,
        state: &mut InstanceState,
        boundary: NodeIx,
    ) -> Result<(), StepError> {
        let sibling = state.next_token_id();
        self.element_started(boundary);
        self.element_completed(boundary);
        self.leave_single(state, sibling, boundary)
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

    /// Open a subscription for the message arm at `element` — a catch, a
    /// receive task or a message boundary, all three through here —
    /// evaluating its correlation key from the variables **now** (arm time),
    /// through [`arm_key`]. When the arm cannot open — an unusable key, or a
    /// duplicate `(message, key)` in this instance — the instance freezes as
    /// an incident instead of waiting forever. A boundary's freeze parks its
    /// host's token **at the boundary element**, exactly as `arm_timer`'s
    /// does, so inspection names the arm that could not be made.
    fn subscribe(
        &mut self,
        state: &mut InstanceState,
        token: TokenId,
        element: NodeIx,
    ) -> Option<SubscriptionId> {
        let (message, key) = match arm_key(self.proc, state, &state.variables, element, None) {
            Ok(arm) => arm,
            Err(failure) => {
                let at = self.proc.node_id(element).to_string();
                self.events.push(match failure {
                    ArmFailure::Unusable { name } => Event::CorrelationFailed { element: at, name },
                    ArmFailure::Duplicate { message, key } => Event::DuplicateSubscription {
                        element: at,
                        message,
                        key,
                    },
                });
                self.freeze(state, token, element, None, None);
                return None;
            }
        };
        let id = state.alloc_subscription(SubscriptionState {
            element,
            token,
            message: message.clone(),
            key: key.clone(),
        });
        self.events.push(Event::MessageSubscribed {
            id,
            element: self.proc.node_id(element).to_string(),
            message,
            key,
            token,
        });
        Some(id)
    }

    /// Every incident converges here: withdraw the token's in-flight arms,
    /// park it at the failing element as the cause (`WaitKind::Incident` —
    /// inspection shows *where*, and a repair has one token to resume), and
    /// freeze the instance under the next incident number. Everything else
    /// the freeze stops is collateral, halted where it stood — a parallel
    /// sibling mid-transit at its target, with the flow it was on; a sibling
    /// parked on a decision, awaiting it. Frozen means *nothing advances and
    /// nothing vanishes*: token conservation must survive the freeze or no
    /// repair can resume (docs/design/incident-scope.md, D7). The cause event
    /// is pushed by the caller first; `incident-raised` closes the sequence.
    fn freeze(
        &mut self,
        state: &mut InstanceState,
        token: TokenId,
        element: NodeIx,
        code: Option<String>,
        detail: Option<String>,
    ) {
        self.cancel_attachments(state, token);
        // Close whatever work this token had open, exactly as the other
        // incident paths do (`RaiseError` closes the failing item; a boundary
        // timer cancels its host's). `cancel_attachments` only withdraws
        // timers and subscriptions, so without this a freeze *during* an
        // entry — a boundary whose deadline will not resolve — leaves an
        // `available` work item on a failed instance. Harmless only for as
        // long as claimability requires `status = 'active'`: a repair API
        // that clears the incident would hand a worker an item whose token
        // is parked at `WaitKind::Incident`, and completing it would advance
        // straight past the incident.
        let open: Vec<WorkItemId> = state
            .work_items
            .iter()
            .filter(|(_, w)| w.open && w.token == token)
            .map(|(id, _)| *id)
            .collect();
        for id in open {
            self.cancel_work_item(state, id);
        }
        // A scope this token owns is the empty one its entry had just opened
        // — the one freeze that could take a waiting host, a non-interrupting
        // boundary failing to re-arm, is refused as a delivery instead (D11).
        // It has no owner left once the freeze parks the token as an
        // incident; leaving it behind would project a `rbpmn_scope` row whose
        // token_no points at a token in another wait state, which is
        // precisely what a resume would trip on.
        let owned_scope = match state.tokens.get(&token).map(|t| &t.wait) {
            Some(WaitKind::Scope(child)) => Some(*child),
            _ => None,
        };
        if let Some(child) = owned_scope {
            self.tear_down_scope(state, child);
            state.scopes.remove(&child);
        }
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
        // A sibling parked on a decision freezes with everything else.
        //
        // `WaitKind::Decision` is the one wait state that does not survive a
        // step: the caller answers it before the transaction ends, which is
        // why persistence refuses to write one. A parallel branch that reached
        // a business-rule task *before* another branch froze the instance left
        // exactly that — a Decision token on a Failed instance — and the
        // engine's drain loop then answered it, got `InstanceNotActive`, and
        // rolled the whole transaction back. Not the freeze: the start. The
        // instance was never created, the incident never recorded, and every
        // retry did the same thing.
        //
        // They are halted at their own node, like the queue below, so
        // inspection still shows where each branch stood — and a repair asks
        // their question again rather than starting the element twice.
        let pending: Vec<TokenId> = state
            .tokens
            .iter()
            .filter(|(_, t)| t.wait == WaitKind::Decision)
            .map(|(id, _)| *id)
            .collect();
        for id in pending {
            if let Some(t) = state.tokens.get_mut(&id) {
                t.wait = WaitKind::Halted(Halt::AwaitingDecision);
            }
        }
        for mv in std::mem::take(&mut self.queue) {
            state.tokens.insert(
                mv.token,
                Token {
                    node: mv.node,
                    scope: mv.scope,
                    wait: WaitKind::Halted(Halt::InFlight { via: mv.via }),
                },
            );
        }
        state.status = InstanceStatus::Failed;
        let incident = state.alloc_incident();
        self.events.push(Event::IncidentRaised {
            element: self.proc.node_id(element).to_string(),
            code,
            detail,
            incident,
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
    ///
    /// **Every reaped token's arms are withdrawn with it** — the
    /// `withdraw_arms` call below must stay paired with the `tokens.remove`
    /// beside it. That pairing is the invariant the projection's timer claim
    /// depends on and cannot check for itself: the scheduler re-checks that a
    /// timer *row* still exists under the instance lock, never that the row's
    /// *token* does, so a timer outliving its token is fired against nothing
    /// and wedges the instance on [`StepError::Invariant`]. It is model
    /// checked as `ArmedTimersHaveLiveTokens` in `spec/TimerTeardown.tla`,
    /// and `spec/TimerTeardown_Buggy.cfg` is the counterexample — this was a
    /// real bug (a token removed *before* teardown ran escaped this loop).
    /// Changing what teardown reaps means re-running `just tla`.
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
                self.cancel_work_item(state, id);
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
