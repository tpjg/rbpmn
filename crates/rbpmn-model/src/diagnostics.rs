//! Machine-readable deploy-time diagnostics and the stable rule catalogue.
//!
//! Rule IDs are public API from day one: the bpmnlint plugin, the playground
//! and every fixture assert on them. Never rename an ID — add new ones.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Warn,
    Error,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Severity::Warn => write!(f, "warn"),
            Severity::Error => write!(f, "error"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub rule: String,
    pub element: String,
    pub message: String,
    pub severity: Severity,
}

impl Diagnostic {
    pub fn error(rule: &str, element: impl Into<String>, message: impl Into<String>) -> Self {
        Diagnostic {
            rule: rule.to_string(),
            element: element.into(),
            message: message.into(),
            severity: Severity::Error,
        }
    }

    pub fn warn(rule: &str, element: impl Into<String>, message: impl Into<String>) -> Self {
        Diagnostic {
            rule: rule.to_string(),
            element: element.into(),
            message: message.into(),
            severity: Severity::Warn,
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {} @ {}: {}",
            self.severity, self.rule, self.element, self.message
        )
    }
}

pub fn has_errors(diagnostics: &[Diagnostic]) -> bool {
    diagnostics.iter().any(|d| d.severity == Severity::Error)
}

pub mod rule {
    // Rules from the design brief.
    pub const NO_INCLUSIVE_GATEWAY: &str = "no-inclusive-gateway";
    pub const NO_CALL_ACTIVITY: &str = "no-call-activity";
    pub const NO_UNSUPPORTED_ELEMENT: &str = "no-unsupported-element";
    pub const BALANCED_GATEWAYS: &str = "balanced-gateways";
    pub const SINGLE_START_EVENT: &str = "single-start-event";
    pub const CONDITIONS_ARE_TRIVIAL: &str = "conditions-are-trivial";
    pub const TIMER_ISO8601: &str = "timer-iso8601";
    pub const MESSAGE_HAS_CORRELATION: &str = "message-has-correlation";
    pub const NO_FOREIGN_IMPLEMENTATION: &str = "no-foreign-implementation";
    pub const BOUNDARY_ON_SUPPORTED_HOST: &str = "boundary-on-supported-host";
    pub const NO_IMPLICIT_SPLIT: &str = "no-implicit-split";
    pub const IMPLICIT_MERGE_AFTER_PARALLEL: &str = "implicit-merge-after-parallel";
    // Structural prerequisites added beyond the brief's initial list (documented
    // in the README): the region analysis is only sound on graphs that pass these.
    pub const BPMN_STRUCTURE: &str = "bpmn-structure";
    pub const NO_MIXED_GATEWAY: &str = "no-mixed-gateway";
    pub const EVENT_GATEWAY_STRUCTURE: &str = "event-gateway-structure";
    pub const SERVICE_TASK_TOPIC: &str = "service-task-topic";
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct RuleInfo {
    pub id: &'static str,
    pub severity: Severity,
    pub summary: &'static str,
}

pub const CATALOGUE: &[RuleInfo] = &[
    RuleInfo {
        id: rule::NO_INCLUSIVE_GATEWAY,
        severity: Severity::Error,
        summary: "Inclusive gateways are rejected entirely; use a parallel split with exclusive skip-bypasses per branch.",
    },
    RuleInfo {
        id: rule::NO_CALL_ACTIVITY,
        severity: Severity::Error,
        summary: "Call activities are rejected; definitions interact only via message throw -> message start/catch.",
    },
    RuleInfo {
        id: rule::NO_UNSUPPORTED_ELEMENT,
        severity: Severity::Error,
        summary: "Element outside the supported BPMN subset.",
    },
    RuleInfo {
        id: rule::BALANCED_GATEWAYS,
        severity: Severity::Error,
        summary: "Every parallel split needs a matching join; branch token flow must not escape the split/join region.",
    },
    RuleInfo {
        id: rule::SINGLE_START_EVENT,
        severity: Severity::Error,
        summary: "Exactly one start event per process and per subprocess (v1 simplification).",
    },
    RuleInfo {
        id: rule::CONDITIONS_ARE_TRIVIAL,
        severity: Severity::Error,
        summary: "Sequence-flow conditions must match the tiny condition grammar; exclusive splits need a default flow.",
    },
    RuleInfo {
        id: rule::TIMER_ISO8601,
        severity: Severity::Error,
        summary: "Timer definitions must be valid ISO-8601 dates or durations.",
    },
    RuleInfo {
        id: rule::MESSAGE_HAS_CORRELATION,
        severity: Severity::Error,
        summary: "Every message start/catch/throw declares a named message and an rbpmn:correlationKey JSON pointer.",
    },
    RuleInfo {
        id: rule::NO_FOREIGN_IMPLEMENTATION,
        severity: Severity::Warn,
        summary: "Service task bound only via vendor-namespace attributes, which rbpmn ignores.",
    },
    RuleInfo {
        id: rule::BOUNDARY_ON_SUPPORTED_HOST,
        severity: Severity::Error,
        summary: "Boundary events only on tasks/subprocesses we support; error boundaries on service tasks/subprocesses.",
    },
    RuleInfo {
        id: rule::NO_IMPLICIT_SPLIT,
        severity: Severity::Error,
        summary: "Activities must have at most one outgoing sequence flow; splitting happens at explicit gateways.",
    },
    RuleInfo {
        id: rule::IMPLICIT_MERGE_AFTER_PARALLEL,
        severity: Severity::Warn,
        summary: "Implicit merge receiving concurrent tokens from a parallel split: the 'task runs twice' trap.",
    },
    RuleInfo {
        id: rule::BPMN_STRUCTURE,
        severity: Severity::Error,
        summary: "Well-formedness: resolvable references, flow cardinalities, connectivity, unique ids.",
    },
    RuleInfo {
        id: rule::NO_MIXED_GATEWAY,
        severity: Severity::Error,
        summary: "A gateway must either split or join, not both (keeps region analysis decidable).",
    },
    RuleInfo {
        id: rule::EVENT_GATEWAY_STRUCTURE,
        severity: Severity::Error,
        summary: "Event-based gateways race message/timer catch events or receive tasks, each with exactly one incoming flow.",
    },
    RuleInfo {
        id: rule::SERVICE_TASK_TOPIC,
        severity: Severity::Error,
        summary: "Service tasks must declare their work-item topic via rbpmn:topic.",
    },
];
