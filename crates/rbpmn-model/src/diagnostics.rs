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
    pub const CONDITIONS_FEEL_SUBSET: &str = "conditions-feel-subset";
    pub const TIMER_ISO8601: &str = "timer-iso8601";
    pub const TIMER_EXPRESSION: &str = "timer-expression";
    pub const MESSAGE_HAS_CORRELATION: &str = "message-has-correlation";
    pub const NO_FOREIGN_IMPLEMENTATION: &str = "no-foreign-implementation";
    /// Deploy-time rule checked against *registration state* (map_topic /
    /// declare_topic), so it cannot fire from pure lint(xml); the id is
    /// reserved here, enforcement lands with the engine (phase 2).
    pub const UNRESOLVED_TOPIC: &str = "unresolved-topic";
    pub const BOUNDARY_ON_SUPPORTED_HOST: &str = "boundary-on-supported-host";
    pub const NO_IMPLICIT_SPLIT: &str = "no-implicit-split";
    pub const IMPLICIT_MERGE_AFTER_PARALLEL: &str = "implicit-merge-after-parallel";
    // Structural prerequisites added beyond the brief's initial list (documented
    // in the README): the region analysis is only sound on graphs that pass these.
    pub const BPMN_STRUCTURE: &str = "bpmn-structure";
    pub const NO_MIXED_GATEWAY: &str = "no-mixed-gateway";
    pub const EVENT_GATEWAY_STRUCTURE: &str = "event-gateway-structure";
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
        id: rule::CONDITIONS_FEEL_SUBSET,
        severity: Severity::Error,
        summary: "Sequence-flow conditions must be in the strict FEEL subset (name op literal, and/or, parentheses); exclusive splits need a default flow.",
    },
    RuleInfo {
        id: rule::TIMER_ISO8601,
        severity: Severity::Error,
        summary: "Timer definitions must be a valid ISO-8601 date or duration, or a FEEL qualified name naming one in the variable document.",
    },
    RuleInfo {
        id: rule::TIMER_EXPRESSION,
        severity: Severity::Warn,
        summary: "A timer read from the variable document cannot be validated ahead of time; if it does not resolve to a valid ISO-8601 value the instance raises an incident there.",
    },
    RuleInfo {
        id: rule::MESSAGE_HAS_CORRELATION,
        severity: Severity::Error,
        summary: "Every message start/catch/throw references a named message; correlation bindings (FEEL qualified names) are registered in code and checked at deploy.",
    },
    RuleInfo {
        id: rule::NO_FOREIGN_IMPLEMENTATION,
        severity: Severity::Warn,
        summary: "Service task carries vendor implementation attributes (camunda/zeebe/flowable), which rbpmn ignores — topics are bound at registration.",
    },
    RuleInfo {
        id: rule::UNRESOLVED_TOPIC,
        severity: Severity::Error,
        summary: "Every service task's topic (map_topic, default: element id) must have a registered handler or a declared external-worker topic — checked at deploy against registration state.",
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
];
