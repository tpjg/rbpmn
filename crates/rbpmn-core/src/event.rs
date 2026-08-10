//! Events: the single output of the semantic core.
//!
//! One list serves three purposes: it is the projection's to-do list (create
//! this work-item row, mark the instance completed), the append-only history
//! (filtered by configured history level at write time), and the golden
//! trace the scenario fixtures assert line by line. The `Display` format is
//! that golden format — stable, like rule IDs.

use crate::compile::WorkKind;
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
        element: String,
        work_kind: WorkKind,
        topic: String,
    },
    WorkItemCompleted {
        element: String,
    },
    WorkItemCancelled {
        element: String,
    },
    VariablesPatched {
        patch: Value,
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
            } => {
                write!(f, "work-item-created {element} {work_kind} {topic}")
            }
            Event::WorkItemCompleted { element } => write!(f, "work-item-completed {element}"),
            Event::WorkItemCancelled { element } => write!(f, "work-item-cancelled {element}"),
            Event::VariablesPatched { .. } => write!(f, "variables-patched"),
            Event::InstanceCompleted => write!(f, "instance-completed"),
            Event::InstanceTerminated => write!(f, "instance-terminated"),
        }
    }
}
