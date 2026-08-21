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
    /// Deploy-time rule checked against *registration state*
    /// (`Bindings::topic` against `declare_topic`), so it cannot fire from
    /// pure `lint(xml)`; the id is reserved here and the engine enforces it.
    pub const UNRESOLVED_TOPIC: &str = "unresolved-topic";
    pub const BOUNDARY_ON_SUPPORTED_HOST: &str = "boundary-on-supported-host";
    /// A non-interrupting boundary spawns a *second* token beside its host's,
    /// which no block-structure proof covers: it entered through no split.
    /// So its path must be a **side path** — disjoint from everything else in
    /// the scope, ending at its own end event. Structural (L1), like every
    /// other token-conservation rule.
    pub const BOUNDARY_SIDE_PATH: &str = "boundary-side-path";
    /// A message arm (catch, receive task, message boundary) on a side path
    /// is armed once per activation of the non-interrupting boundary, and an
    /// earlier activation's arm may still be open: the second arm is a
    /// duplicate-(message, key) freeze unless each activation changes the
    /// key (a delivery patch can). Not always wrong, so a warning — with the
    /// consequence named.
    pub const SIDE_PATH_MESSAGE_ARM: &str = "side-path-message-arm";
    /// Two message arms for the same message *and* the same correlation
    /// binding that can be live at once. Deploy-time (L2) like
    /// `unresolved-decision`: with *different* bindings both arms resolve to
    /// different keys and both may legitimately be live, so only the manifest
    /// decides — and the manifest is never in the XML. Enforced in
    /// `rbpmn_core::compile`, reported through `check_deployable`.
    pub const AMBIGUOUS_MESSAGE_ARM: &str = "ambiguous-message-arm";
    pub const NO_IMPLICIT_SPLIT: &str = "no-implicit-split";
    pub const IMPLICIT_MERGE_AFTER_PARALLEL: &str = "implicit-merge-after-parallel";
    // Structural prerequisites added beyond the brief's initial list (documented
    // in the README): the region analysis is only sound on graphs that pass these.
    pub const BPMN_STRUCTURE: &str = "bpmn-structure";
    pub const NO_MIXED_GATEWAY: &str = "no-mixed-gateway";
    pub const EVENT_GATEWAY_STRUCTURE: &str = "event-gateway-structure";

    // DMN. The ids live here because there is one `Diagnostic` type and one
    // catalogue; the rules themselves are implemented in `rbpmn-dmn`, which
    // is where dsntk is allowed and nothing upstream of it may depend on
    // (docs/dmn.md, D1). These constants are strings, so this costs
    // `rbpmn-model` no dependency and keeps rule ids a single namespace.
    pub const DMN_VALIDATES: &str = "dmn-validates";
    pub const FEEL_PARSES: &str = "feel-parses";
    pub const FEEL_DETERMINISTIC: &str = "feel-deterministic";
    /// A business-rule task's manifest binding: present, well-formed, and
    /// naming a decision the bundle actually exposes.
    pub const DECISION_HAS_BINDING: &str = "decision-has-binding";
    pub const UNRESOLVED_DECISION: &str = "unresolved-decision";
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
        summary: "Every message start/catch/throw/boundary references a named message; correlation bindings (FEEL qualified names) are registered in code and checked at deploy — a message boundary needs its own, keyed by the boundary's element id, never the host's.",
    },
    RuleInfo {
        id: rule::NO_FOREIGN_IMPLEMENTATION,
        severity: Severity::Warn,
        summary: "Service task carries vendor implementation attributes (camunda/zeebe/flowable), which rbpmn ignores — topics are bound at registration.",
    },
    RuleInfo {
        id: rule::UNRESOLVED_TOPIC,
        severity: Severity::Error,
        summary: "Every service task's topic (`Bindings::topic`, default: element id) must have a registered handler or a declared external-worker topic — checked at deploy against registration state.",
    },
    RuleInfo {
        id: rule::BOUNDARY_ON_SUPPORTED_HOST,
        severity: Severity::Error,
        summary: "Boundary events only on tasks/subprocesses we support; error boundaries on service tasks/subprocesses.",
    },
    RuleInfo {
        id: rule::BOUNDARY_SIDE_PATH,
        severity: Severity::Error,
        summary: "A non-interrupting boundary starts a side path: it ends at its own end event, never merges into another flow or reaches a parallel join, and carries no parallel block of its own (the boundary can fire again while an earlier side token is still inside it — wrap the block in a subprocess).",
    },
    RuleInfo {
        id: rule::SIDE_PATH_MESSAGE_ARM,
        severity: Severity::Warn,
        summary: "A message arm on a side path is armed once per activation of its non-interrupting boundary; unless each activation changes the correlation key, the second arm freezes the instance (duplicate-subscription).",
    },
    RuleInfo {
        id: rule::AMBIGUOUS_MESSAGE_ARM,
        severity: Severity::Error,
        summary: "Two message arms for the same message and the same correlation binding can be live at once (two boundaries on one host, a host and its own boundary, a subprocess boundary and a catch inside it): every delivery would be ambiguous, so deploy refuses it.",
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
        id: rule::DMN_VALIDATES,
        severity: Severity::Error,
        summary: "A bundled DMN artifact must parse and its decision logic must build; a decision that cannot be compiled cannot be deployed.",
    },
    RuleInfo {
        id: rule::FEEL_PARSES,
        severity: Severity::Error,
        summary: "Every FEEL expression in a DMN artifact must parse — literal expressions, decision-table entries, item-definition constraints and the rest.",
    },
    RuleInfo {
        id: rule::DECISION_HAS_BINDING,
        severity: Severity::Error,
        summary: "A business-rule task's decision binding lives in the manifest (never in the XML) and must be well-formed: a decision name, and a FEEL qualified name for where the answer lands.",
    },
    RuleInfo {
        id: rule::UNRESOLVED_DECISION,
        severity: Severity::Error,
        summary: "Every bound decision must name exactly one invocable in the bundled DMN artifacts. Unlike unresolved-topic this needs no environment: decisions travel inside the deployment.",
    },
    RuleInfo {
        id: rule::FEEL_DETERMINISTIC,
        severity: Severity::Error,
        summary: "Decisions must be deterministic and self-contained: no now()/today(), and no external Java or PMML functions. Time enters as an input, never from a clock.",
    },
];
