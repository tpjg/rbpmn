//! Events: the single output of the semantic core.
//!
//! One list serves three purposes: it is the projection's to-do list (create
//! this work-item row, mark the instance completed), the append-only history,
//! and the golden trace the scenario fixtures assert line by line. The
//! `Display` format is that golden format — stable, like rule IDs.
//!
//! Every event is written; nothing is filtered at write time. The brief's
//! "history level" (per-definition event-kind filtering) is a roadmap item,
//! deliberately not folded into retention: it changes the stream's
//! *completeness* contract, because a consumer could no longer tell "did not
//! happen" from "was not recorded". Retention makes a narrower claim — this
//! was here, it was deleted, here is the floor.

use crate::compile::{TimerDue, WorkKind};
use crate::state::{SubscriptionId, TimerId, WorkItemId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Event {
    /// ...carrying the variables it started with.
    ///
    /// Without this the history is **not self-contained**: every later change
    /// is recorded (a patch in `variables-patched`, an answer in
    /// `decision-evaluated`) but the ground they apply to was not, so
    /// reconstructing the document at any point meant having the `Start`
    /// command from somewhere outside the log. The storm's replay driver had
    /// to supply it; nothing else could.
    ///
    /// With it, the document at any point in an instance's life is *the start,
    /// plus the changes up to there* — a forward fold. That is the cheap
    /// direction. The alternative, unwinding from the current document by
    /// inverting patches, is not generally possible: RFC 7386 `null` deletes a
    /// member without recording what it deleted, so a patch is not invertible
    /// from the patch alone. Compensation, were it ever built, wants the
    /// forward fold for exactly that reason.
    ///
    /// Not in `Display` — the trace line stays `instance-started`, and this is
    /// business data of unbounded shape. Same split as `decision-evaluated`.
    InstanceStarted {
        variables: Value,
    },
    ElementStarted {
        element: String,
    },
    ElementCompleted {
        element: String,
    },
    FlowTaken {
        flow: String,
    },
    WorkItemCreated {
        id: WorkItemId,
        element: String,
        work_kind: WorkKind,
        topic: String,
    },
    WorkItemCompleted {
        id: WorkItemId,
        element: String,
    },
    WorkItemCancelled {
        id: WorkItemId,
        element: String,
    },
    WorkItemFailed {
        id: WorkItemId,
        element: String,
        code: Option<String>,
    },
    /// A business-rule task needs its decision evaluated. The projection
    /// answers this inside the same transaction.
    DecisionRequested {
        element: String,
        decision: String,
    },
    /// ...and the answer, which is what a replay reads back instead of
    /// evaluating anything.
    ///
    /// The failure reason stays out of `Display` — that is stable API, and a
    /// reason improved later must not break a golden trace, the
    /// `timer-resolve-failed` precedent. It is in the *payload*, which is
    /// serialised whole into `/v1/events`, because a null answer with no
    /// explanation leaves an operator nothing to go on: FEEL nulls a decision
    /// whose input was the wrong type exactly as it nulls one where no rule
    /// matched, and dsntk's text is the only thing that tells them apart.
    DecisionEvaluated {
        element: String,
        result: Value,
        /// Why the answer is null, when the evaluator said. Never set for a
        /// non-null answer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    IncidentRaised {
        element: String,
        code: Option<String>,
        /// What froze it, in prose, when the freezing path had prose to give
        /// — a failed decision does, and it has no `code` because DMN has
        /// none to give. Same split as above: payload, not `Display`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    VariablesPatched {
        patch: Value,
    },
    /// A timer was armed on `element` (a catch or boundary event) for
    /// `token`. The projection turns `due` into an absolute `due_at` using
    /// database time. Self-contained on purpose: a timer armed and torn
    /// down in the same step (a racing terminate) is gone from the state by
    /// the time the projection reads the events.
    TimerArmed {
        id: TimerId,
        element: String,
        due: TimerDue,
        token: crate::state::TokenId,
        /// A cycle re-arming after a fire names the timer it continues: the
        /// projection computes the next instant as *that timer's due + the
        /// period*, never from the time the fire happened to run, so a late
        /// scheduler does not drift the schedule. `None` for a first arm,
        /// which is computed from database time. Payload, not `Display`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        continues: Option<TimerId>,
        /// A cycle's fires left, the armed one included; carried here for
        /// the same reason as `token`: a timer armed and withdrawn in one
        /// step is gone from the state by the time the row is written.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remaining: Option<u32>,
    },
    TimerFired {
        id: TimerId,
        element: String,
    },
    TimerCancelled {
        id: TimerId,
        element: String,
    },
    /// A message subscription opened on `element`; `key` is the correlation
    /// key value evaluated from the variables at arm time. Carries `token`
    /// for the same reason as [`Event::TimerArmed`].
    MessageSubscribed {
        id: SubscriptionId,
        element: String,
        message: String,
        key: String,
        token: crate::state::TokenId,
    },
    MessageReceived {
        id: SubscriptionId,
        element: String,
        message: String,
    },
    SubscriptionCancelled {
        id: SubscriptionId,
        element: String,
        message: String,
    },
    /// The correlation key did not evaluate to a string or an exact integer
    /// — the subscription could never match (floats have no canonical
    /// spelling across a jsonb round-trip), so the instance freezes as an
    /// incident instead of waiting forever ("seems to run" is the enemy).
    CorrelationFailed {
        element: String,
        name: String,
    },
    /// A timer whose deadline is read from the variable document could not be
    /// resolved: the name is missing, holds a non-string, or holds a string
    /// that is not a valid ISO-8601 value. Arming freezes the instance as an
    /// incident rather than firing at a guessed time or waiting forever —
    /// `reason` is what an operator reads to fix it. Sibling of
    /// [`Event::CorrelationFailed`], which fails the same way for the same
    /// reason: both resolve a qualified name at arm time.
    TimerResolveFailed {
        element: String,
        name: String,
        reason: String,
    },
    /// A second open subscription for the same (message, key) in one
    /// instance: every delivery would be permanently ambiguous, so arming
    /// freezes the instance as an incident.
    DuplicateSubscription {
        element: String,
        message: String,
        key: String,
    },
    InstanceCompleted,
    InstanceTerminated,
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::InstanceStarted { .. } => write!(f, "instance-started"),
            Event::ElementStarted { element } => write!(f, "element-started {element}"),
            Event::ElementCompleted { element } => write!(f, "element-completed {element}"),
            Event::FlowTaken { flow } => write!(f, "flow-taken {flow}"),
            Event::WorkItemCreated {
                element,
                work_kind,
                topic,
                ..
            } => {
                write!(f, "work-item-created {element} {work_kind} {topic}")
            }
            Event::WorkItemCompleted { element, .. } => write!(f, "work-item-completed {element}"),
            Event::WorkItemCancelled { element, .. } => write!(f, "work-item-cancelled {element}"),
            Event::WorkItemFailed { element, code, .. } => match code {
                Some(code) => write!(f, "work-item-failed {element} {code}"),
                None => write!(f, "work-item-failed {element}"),
            },
            Event::DecisionRequested { element, decision } => {
                write!(f, "decision-requested {element} {decision}")
            }
            // The *answer* is not in the trace line, only that one arrived:
            // a decision's result is business data of unbounded shape, and a
            // golden trace is a control-flow record. `variables-patched`
            // already marks that the document changed.
            Event::DecisionEvaluated { element, .. } => {
                write!(f, "decision-evaluated {element}")
            }
            Event::IncidentRaised { element, code, .. } => match code {
                Some(code) => write!(f, "incident-raised {element} {code}"),
                None => write!(f, "incident-raised {element}"),
            },
            Event::VariablesPatched { .. } => write!(f, "variables-patched"),
            Event::TimerArmed { element, due, .. } => {
                write!(f, "timer-armed {element} {due}")
            }
            Event::TimerFired { element, .. } => write!(f, "timer-fired {element}"),
            Event::TimerCancelled { element, .. } => write!(f, "timer-cancelled {element}"),
            Event::MessageSubscribed {
                element,
                message,
                key,
                ..
            } => write!(f, "message-subscribed {element} {message} {key}"),
            Event::MessageReceived {
                element, message, ..
            } => write!(f, "message-received {element} {message}"),
            Event::SubscriptionCancelled {
                element, message, ..
            } => write!(f, "subscription-cancelled {element} {message}"),
            Event::CorrelationFailed { element, name } => {
                write!(f, "correlation-failed {element} {name}")
            }
            Event::TimerResolveFailed { element, name, .. } => {
                // The reason stays out of the golden format: it is prose, it
                // will improve, and a trace should not break when it does.
                // Inspection shows it in full from the stored payload.
                write!(f, "timer-resolve-failed {element} {name}")
            }
            Event::DuplicateSubscription {
                element,
                message,
                key,
            } => {
                write!(f, "duplicate-subscription {element} {message} {key}")
            }
            Event::InstanceCompleted => write!(f, "instance-completed"),
            Event::InstanceTerminated => write!(f, "instance-terminated"),
        }
    }
}
