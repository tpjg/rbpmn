//! Events: the single output of the semantic core.
//!
//! One list serves three purposes: it is the projection's to-do list (create
//! this work-item row, mark the instance completed), the append-only history
//! (filtered by configured history level at write time), and the golden
//! trace the scenario fixtures assert line by line. The `Display` format is
//! that golden format — stable, like rule IDs.

use crate::compile::{TimerDue, WorkKind};
use crate::state::{SubscriptionId, TimerId, WorkItemId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Event {
    InstanceStarted,
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
    IncidentRaised {
        element: String,
        code: Option<String>,
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
            Event::InstanceStarted => write!(f, "instance-started"),
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
            Event::IncidentRaised { element, code } => match code {
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
