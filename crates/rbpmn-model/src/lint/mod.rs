//! The linter: whitelist enforcement plus structural analysis, producing
//! machine-readable diagnostics. Staged per scope:
//!
//!   1. element rules (whitelist, bindings, timers, messages)
//!   2. graph rules (well-formedness, conditions, event gateways, boundaries)
//!   3. region analysis (`balanced-gateways`) — only when 1+2 found no errors
//!      in the scope, because its assumptions depend on them.

mod regions;
mod structure;

use crate::condition;
use crate::diagnostics::{Diagnostic, Severity, rule};
use crate::iso8601;
use crate::model::*;
use std::collections::BTreeMap;
use structure::Graph;

pub fn lint(defs: &Definitions) -> Vec<Diagnostic> {
    let mut out = Vec::new();

    for id in &defs.missing_ids {
        out.push(Diagnostic::error(
            rule::BPMN_STRUCTURE,
            id,
            "element is missing its required id attribute",
        ));
    }

    duplicate_ids(defs, &mut out);

    for p in &defs.processes {
        lint_scope(defs, &p.body, &p.id, true, &mut out);
    }

    out
}

fn lint_scope(
    defs: &Definitions,
    scope: &FlowScope,
    owner: &str,
    is_process: bool,
    out: &mut Vec<Diagnostic>,
) {
    let start = out.len();

    element_rules(defs, scope, is_process, out);

    let g = Graph::build(scope);
    if !g.has_duplicate_ids {
        structure::check(&g, owner, out);
        condition_rules(&g, out);
        event_gateway_rules(&g, out);
        boundary_rules(defs, &g, out);

        let scope_clean = !out[start..].iter().any(|d| d.severity == Severity::Error);
        if scope_clean {
            regions::check(&g, out);
        }
    }

    for node in &scope.nodes {
        if let NodeKind::SubProcess(sp) = &node.kind {
            // Event subprocesses are already rejected wholesale; linting their
            // body against embedded-subprocess rules would mislead.
            if !sp.triggered_by_event {
                lint_scope(defs, &sp.body, &node.id, false, out);
            }
        }
    }
}

fn duplicate_ids(defs: &Definitions, out: &mut Vec<Diagnostic>) {
    let mut counts: BTreeMap<&str, u32> = BTreeMap::new();

    fn walk<'a>(scope: &'a FlowScope, counts: &mut BTreeMap<&'a str, u32>) {
        for n in &scope.nodes {
            *counts.entry(n.id.as_str()).or_default() += 1;
            if let NodeKind::SubProcess(sp) = &n.kind {
                walk(&sp.body, counts);
            }
        }
        for f in &scope.flows {
            *counts.entry(f.id.as_str()).or_default() += 1;
        }
    }

    for p in &defs.processes {
        *counts.entry(p.id.as_str()).or_default() += 1;
        walk(&p.body, &mut counts);
    }
    for m in &defs.messages {
        *counts.entry(m.id.as_str()).or_default() += 1;
    }
    for e in &defs.errors {
        *counts.entry(e.id.as_str()).or_default() += 1;
    }

    for (id, count) in counts {
        if count > 1 {
            out.push(Diagnostic::error(
                rule::BPMN_STRUCTURE,
                id,
                format!("duplicate id: used {count} times — ids must be unique"),
            ));
        }
    }
}

fn element_rules(
    defs: &Definitions,
    scope: &FlowScope,
    is_process: bool,
    out: &mut Vec<Diagnostic>,
) {
    for node in &scope.nodes {
        let id = &node.id;

        if let Some(loop_kind) = &node.loop_kind {
            let msg = match loop_kind {
                LoopKind::MultiInstance => {
                    "multi-instance activities are not supported (planned post-v1)"
                }
                LoopKind::Standard => {
                    "standardLoopCharacteristics is not supported — model the loop \
                     explicitly with an exclusive gateway around the whole block"
                }
            };
            out.push(Diagnostic::error(rule::NO_UNSUPPORTED_ELEMENT, id, msg));
        }

        match &node.kind {
            NodeKind::InclusiveGateway => out.push(Diagnostic::error(
                rule::NO_INCLUSIVE_GATEWAY,
                id,
                "inclusive gateways are not supported: the converging side requires \
                 non-local reachability analysis that cannot be implemented correctly \
                 in general. Rewrite as a parallel split/join whose branches each \
                 start with an exclusive skip-bypass gateway (parallel + skip pattern)",
            )),
            NodeKind::CallActivity => out.push(Diagnostic::error(
                rule::NO_CALL_ACTIVITY,
                id,
                "call activities are not supported: definitions are islands. Deploy \
                 the callee as its own process and interact via a message throw event \
                 to its message start event, correlating replies with a correlation key",
            )),
            NodeKind::Unsupported { tag } => out.push(Diagnostic::error(
                rule::NO_UNSUPPORTED_ELEMENT,
                id,
                unsupported_message(tag),
            )),
            NodeKind::SubProcess(sp) => {
                if sp.triggered_by_event {
                    out.push(Diagnostic::error(
                        rule::NO_UNSUPPORTED_ELEMENT,
                        id,
                        "event subprocesses are not supported (planned for v3)",
                    ));
                }
            }
            NodeKind::ServiceTask { foreign } => {
                if !foreign.is_empty() {
                    out.push(Diagnostic::warn(
                        rule::NO_FOREIGN_IMPLEMENTATION,
                        id,
                        format!(
                            "service task carries vendor implementation attribute(s) \
                             {} which rbpmn ignores — topics are bound at registration \
                             time (map_topic; default topic = the element id), never \
                             in the XML",
                            foreign.join(", ")
                        ),
                    ));
                }
            }
            NodeKind::Start(trigger) => match trigger {
                StartTrigger::None => {}
                StartTrigger::Message(message_ref) => {
                    if is_process {
                        check_message(defs, id, message_ref.as_deref(), out);
                    } else {
                        out.push(Diagnostic::error(
                            rule::NO_UNSUPPORTED_ELEMENT,
                            id,
                            "message start events are only supported on top-level \
                             processes; an embedded subprocess starts with a plain \
                             (none) start event",
                        ));
                    }
                }
                StartTrigger::Timer(_) => out.push(Diagnostic::error(
                    rule::NO_UNSUPPORTED_ELEMENT,
                    id,
                    "timer start events are not supported in v1 — start instances \
                     from application code, or via a message",
                )),
                StartTrigger::Unsupported { tag } => out.push(Diagnostic::error(
                    rule::NO_UNSUPPORTED_ELEMENT,
                    id,
                    format!("'{tag}' start events are not supported (v1: none, message)"),
                )),
            },
            NodeKind::End(kind) => match kind {
                EndKind::None | EndKind::Terminate => {}
                EndKind::Message(message_ref) => {
                    check_message(defs, id, message_ref.as_deref(), out)
                }
                EndKind::Unsupported { tag } => out.push(Diagnostic::error(
                    rule::NO_UNSUPPORTED_ELEMENT,
                    id,
                    format!("'{tag}' end events are not supported (v1: none, terminate, message)"),
                )),
            },
            NodeKind::Catch(trigger) => match trigger {
                CatchTrigger::Message(message_ref) => {
                    check_message(defs, id, message_ref.as_deref(), out)
                }
                CatchTrigger::Timer(spec) => check_timer(id, spec, out),
                CatchTrigger::Unsupported { tag } => out.push(Diagnostic::error(
                    rule::NO_UNSUPPORTED_ELEMENT,
                    id,
                    format!(
                        "'{tag}' intermediate catch events are not supported \
                         (v1: message, timer)"
                    ),
                )),
            },
            NodeKind::Throw(kind) => match kind {
                ThrowKind::Message(message_ref) => {
                    check_message(defs, id, message_ref.as_deref(), out)
                }
                ThrowKind::None => out.push(Diagnostic::error(
                    rule::NO_UNSUPPORTED_ELEMENT,
                    id,
                    "none intermediate throw events have no effect and are not supported",
                )),
                ThrowKind::Unsupported { tag } => out.push(Diagnostic::error(
                    rule::NO_UNSUPPORTED_ELEMENT,
                    id,
                    format!("'{tag}' throw events are not supported (v1: message)"),
                )),
            },
            NodeKind::Boundary(b) => {
                if !b.cancel_activity {
                    out.push(Diagnostic::error(
                        rule::NO_UNSUPPORTED_ELEMENT,
                        id,
                        "non-interrupting boundary events are not supported in v1 \
                         (planned for v2)",
                    ));
                }
                match &b.trigger {
                    BoundaryTrigger::Timer(spec) => check_timer(id, spec, out),
                    BoundaryTrigger::Error { .. } => {}
                    BoundaryTrigger::Message(_) => out.push(Diagnostic::error(
                        rule::NO_UNSUPPORTED_ELEMENT,
                        id,
                        "message boundary events are not supported in v1 \
                         (v1 boundary events: timer, error)",
                    )),
                    BoundaryTrigger::None => out.push(Diagnostic::error(
                        rule::BPMN_STRUCTURE,
                        id,
                        "boundary event requires an event definition (timer or error)",
                    )),
                    BoundaryTrigger::Unsupported { tag } => out.push(Diagnostic::error(
                        rule::NO_UNSUPPORTED_ELEMENT,
                        id,
                        format!("'{tag}' boundary events are not supported (v1: timer, error)"),
                    )),
                }
            }
            NodeKind::ReceiveTask { message_ref } => {
                check_message(defs, id, message_ref.as_deref(), out)
            }
            NodeKind::UserTask
            | NodeKind::ExclusiveGateway { .. }
            | NodeKind::ParallelGateway
            | NodeKind::EventBasedGateway => {}
        }
    }
}

fn unsupported_message(tag: &str) -> String {
    let hint = match tag {
        "scriptTask" => " — compute in application code (a service task handler) instead",
        "sendTask" => " — use a message intermediate throw event instead",
        "manualTask" => " — use a user task instead",
        "businessRuleTask" => {
            " — planned post-v1 via DMN; until then compute the \
             decision in application code and store the result as a variable"
        }
        "task" => {
            " — the abstract task has no execution semantics; use a \
             service, user or receive task"
        }
        "complexGateway" => " — model the routing explicitly with exclusive/parallel gateways",
        _ => "",
    };
    format!("'{tag}' is not in the supported BPMN subset{hint}")
}

/// A timer spec is a literal ISO-8601 value — validated here, before anything
/// runs — or, if it does not parse as one, a FEEL qualified name naming a
/// value in the variable document. BPMN types `timeDate`/`timeDuration` as
/// `tExpression`, not as a string, so the second form is standard BPMN
/// needing no extension: the same mechanism `conditionExpression` uses.
///
/// **Literal first, always.** An earlier version required
/// `xsi:type="bpmn:tFormalExpression"` to opt into the reference form, on the
/// theory that the marker signals intent. It does not: bpmn-moddle emits it
/// for *any* expression object, so every bpmn-js modeler — the editor in this
/// repo, Camunda Modeler — writes it on ordinary literal durations. Keying
/// off it turned `P3D` typed into a properties panel into a variable lookup
/// named `P3D`. Parse order is the honest signal; the marker is ignored.
///
/// The reference form can only be a **warning**. Whether `order.sla` holds a
/// valid duration is unknowable at deploy — variables are one opaque document
/// with no declarations — so the honest thing to say is what the consequence
/// will be, not that something is wrong. Erroring would also be wrong for the
/// standalone linter, which lints models targeting other engines, where this
/// is ordinary valid BPMN.
///
/// The warning carries the ISO-8601 complaint that made the text fall through
/// (`P30X` is a mistyped duration *and* a syntactically valid qualified
/// name). That is what keeps a typo legible: the author reads why it is not a
/// duration and what it will be treated as instead, in one line.
fn check_timer(id: &str, spec: &TimerSpec, out: &mut Vec<Diagnostic>) {
    let (what, text, literal) = match spec {
        TimerSpec::Date(s) => ("timeDate", s, iso8601::validate_datetime(s)),
        TimerSpec::Duration(s) => ("timeDuration", s, iso8601::validate_duration(s)),
        TimerSpec::Cycle(_) => {
            out.push(Diagnostic::error(
                rule::NO_UNSUPPORTED_ELEMENT,
                id,
                "repeating timer cycles (timeCycle) are not supported in v1 — planned \
                 for v2 with non-interrupting boundary timers",
            ));
            return;
        }
        TimerSpec::Missing => {
            out.push(Diagnostic::error(
                rule::TIMER_ISO8601,
                id,
                "timer event definition needs a timeDate or timeDuration",
            ));
            return;
        }
    };
    let Err(why) = literal else { return };
    match condition::parse_qname(text) {
        Ok(path) => out.push(Diagnostic::warn(
            rule::TIMER_EXPRESSION,
            id,
            format!(
                "{what} '{text}' is not a literal ISO-8601 value ({why}), so it is read \
                 as the variable '{}' when the timer is armed — rbpmn cannot check ahead \
                 of time that it holds a valid value, and if it does not, this element \
                 raises an incident rather than firing",
                path.join(".")
            ),
        )),
        Err(_) => out.push(Diagnostic::error(
            rule::TIMER_ISO8601,
            id,
            format!(
                "invalid {what}: {why} — and '{text}' is not a FEEL qualified name \
                 naming one in the variable document either"
            ),
        )),
    }
}

/// The XML side of `message-has-correlation`: the element must reference a
/// *named* message. The correlation binding itself (a FEEL qualified name
/// into the instance variables) is registered in code (`map_correlation`)
/// and checked at deploy against registration state — never in the XML.
fn check_message(
    defs: &Definitions,
    id: &str,
    message_ref: Option<&str>,
    out: &mut Vec<Diagnostic>,
) {
    match message_ref {
        None => out.push(Diagnostic::error(
            rule::MESSAGE_HAS_CORRELATION,
            id,
            "message event must reference a message definition (messageRef)",
        )),
        Some(mref) => match defs.messages.iter().find(|m| m.id == mref) {
            None => out.push(Diagnostic::error(
                rule::MESSAGE_HAS_CORRELATION,
                id,
                format!("messageRef '{mref}' does not resolve to a message definition"),
            )),
            Some(m) => {
                if m.name.as_deref().is_none_or(str::is_empty) {
                    out.push(Diagnostic::error(
                        rule::MESSAGE_HAS_CORRELATION,
                        id,
                        format!(
                            "message '{mref}' needs a name — the name is what \
                             correlate() addresses"
                        ),
                    ));
                }
            }
        },
    }
}

fn condition_rules(g: &Graph, out: &mut Vec<Diagnostic>) {
    let is_exclusive_split =
        |v: usize| matches!(g.node(v).kind, NodeKind::ExclusiveGateway { .. }) && g.out_deg(v) > 1;

    // Conditions are only legal on the outgoing flows of an exclusive split;
    // those flows are checked exhaustively below.
    for (fi, endpoints) in g.endpoints.iter().enumerate() {
        let Some((src, _)) = endpoints else { continue };
        let flow = g.flow(fi);
        if flow.condition.is_some() && !is_exclusive_split(*src) {
            out.push(Diagnostic::error(
                rule::CONDITIONS_FEEL_SUBSET,
                &flow.id,
                format!(
                    "conditions are only supported on the outgoing flows of an \
                     exclusive gateway split (this flow leaves a {})",
                    g.node(*src).kind.describe()
                ),
            ));
        }
    }

    for v in 0..g.scope.nodes.len() {
        let NodeKind::ExclusiveGateway { default_flow } = &g.node(v).kind else {
            continue;
        };
        if g.out_deg(v) <= 1 {
            continue;
        }
        let gateway_id = &g.node(v).id;

        let default_fi = match default_flow.as_deref() {
            None => {
                out.push(Diagnostic::error(
                    rule::CONDITIONS_FEEL_SUBSET,
                    gateway_id,
                    "exclusive split needs a default flow (the `default` attribute) \
                     so no token can get stuck when every condition is false",
                ));
                None
            }
            Some(d) => {
                let fi = g.flow_out[v]
                    .iter()
                    .find(|&&fi| g.flow(fi).id == d)
                    .copied();
                if fi.is_none() {
                    out.push(Diagnostic::error(
                        rule::CONDITIONS_FEEL_SUBSET,
                        gateway_id,
                        format!("default flow '{d}' is not an outgoing flow of this gateway"),
                    ));
                }
                fi
            }
        };

        for &fi in &g.flow_out[v] {
            let flow = g.flow(fi);
            if Some(fi) == default_fi {
                if flow.condition.is_some() {
                    out.push(Diagnostic::error(
                        rule::CONDITIONS_FEEL_SUBSET,
                        &flow.id,
                        "the default flow must not carry a condition",
                    ));
                }
                continue;
            }
            match &flow.condition {
                None => out.push(Diagnostic::error(
                    rule::CONDITIONS_FEEL_SUBSET,
                    &flow.id,
                    "flow out of an exclusive split needs a condition \
                     (or mark it as the gateway's default flow)",
                )),
                Some(src) => {
                    if let Err(e) = condition::parse(src) {
                        out.push(Diagnostic::error(
                            rule::CONDITIONS_FEEL_SUBSET,
                            &flow.id,
                            format!(
                                "condition does not match the grammar \
                                 `<json-pointer> <op> <literal>` with and/or: {e}"
                            ),
                        ));
                    }
                }
            }
        }
    }
}

fn event_gateway_rules(g: &Graph, out: &mut Vec<Diagnostic>) {
    // host element id -> its boundary events, built once for the scope
    // (the alternative is rescanning every node per gateway alternative).
    let mut boundaries: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for v in 0..g.scope.nodes.len() {
        if let NodeKind::Boundary(data) = &g.node(v).kind
            && let Some(host) = data.attached_to.as_deref()
        {
            boundaries.entry(host).or_default().push(&g.node(v).id);
        }
    }

    for v in 0..g.scope.nodes.len() {
        if !matches!(g.node(v).kind, NodeKind::EventBasedGateway) {
            continue;
        }
        if g.out_deg(v) < 2 {
            out.push(Diagnostic::error(
                rule::EVENT_GATEWAY_STRUCTURE,
                &g.node(v).id,
                "event-based gateway needs at least two alternatives to race",
            ));
        }
        for &fi in &g.flow_out[v] {
            let t = g.tgt(fi);
            let target = g.node(t);
            let supported_target = matches!(
                target.kind,
                NodeKind::Catch(CatchTrigger::Message(_))
                    | NodeKind::Catch(CatchTrigger::Timer(_))
                    | NodeKind::ReceiveTask { .. }
            );
            if !supported_target {
                out.push(Diagnostic::error(
                    rule::EVENT_GATEWAY_STRUCTURE,
                    &target.id,
                    format!(
                        "event-based gateway alternatives must be message/timer \
                         catch events or receive tasks, found {}",
                        target.kind.describe()
                    ),
                ));
            } else {
                if g.in_deg(t) != 1 {
                    out.push(Diagnostic::error(
                        rule::EVENT_GATEWAY_STRUCTURE,
                        &target.id,
                        "an event-based gateway's target must have exactly one incoming \
                         flow (from the gateway) — it is armed only by the gateway",
                    ));
                }
                // A boundary event on a gateway target could never arm: the
                // gateway holds the token, the target is never *entered*, so
                // the boundary would silently not exist at runtime — the
                // "seems to run" failure mode this linter exists to kill.
                // Only reported for otherwise-valid targets: an unsupported
                // target already has its own diagnostic, and a second one
                // would point the modeller at the wrong fix.
                for boundary in boundaries.get(target.id.as_str()).into_iter().flatten() {
                    out.push(Diagnostic::error(
                        rule::EVENT_GATEWAY_STRUCTURE,
                        *boundary,
                        "boundary events cannot attach to an event-based gateway's \
                         alternative — the gateway itself is the race; model the \
                         timeout as a timer alternative of the gateway instead",
                    ));
                }
            }
        }
    }
}

fn boundary_rules(defs: &Definitions, g: &Graph, out: &mut Vec<Diagnostic>) {
    // (host, error code) -> first boundary claiming it; a second boundary
    // with the same code could never fire — ambiguity we reject loudly.
    let mut error_claims: BTreeMap<(usize, String), String> = BTreeMap::new();
    for v in 0..g.scope.nodes.len() {
        let NodeKind::Boundary(b) = &g.node(v).kind else {
            continue;
        };
        let id = &g.node(v).id;

        let host = match b.attached_to.as_deref() {
            None => {
                out.push(Diagnostic::error(
                    rule::BPMN_STRUCTURE,
                    id,
                    "boundary event must declare attachedToRef",
                ));
                continue;
            }
            Some(h) => match g.idx.get(h) {
                None => {
                    out.push(Diagnostic::error(
                        rule::BPMN_STRUCTURE,
                        id,
                        format!("attachedToRef '{h}' does not resolve within this scope"),
                    ));
                    continue;
                }
                Some(&host) => host,
            },
        };

        let host_kind = &g.node(host).kind;
        if !host_kind.is_supported_boundary_host() {
            out.push(Diagnostic::error(
                rule::BOUNDARY_ON_SUPPORTED_HOST,
                id,
                format!(
                    "boundary events cannot attach to a {} — supported hosts: \
                     service task, user task, receive task, embedded subprocess",
                    host_kind.describe()
                ),
            ));
            continue;
        }

        if let BoundaryTrigger::Error { error_ref } = &b.trigger {
            if !matches!(
                host_kind,
                NodeKind::ServiceTask { .. } | NodeKind::SubProcess(_)
            ) {
                out.push(Diagnostic::error(
                    rule::BOUNDARY_ON_SUPPORTED_HOST,
                    id,
                    "error boundary events attach to service tasks or subprocesses — \
                     v1 errors are raised by service-task failures past their retry budget",
                ));
            }
            match error_ref.as_deref() {
                None => out.push(Diagnostic::error(
                    rule::BPMN_STRUCTURE,
                    id,
                    "error boundary event must reference an error definition (errorRef)",
                )),
                Some(eref) => match defs.errors.iter().find(|e| e.id == eref) {
                    None => out.push(Diagnostic::error(
                        rule::BPMN_STRUCTURE,
                        id,
                        format!("errorRef '{eref}' does not resolve to an error definition"),
                    )),
                    Some(e) => match e.code.as_deref() {
                        None | Some("") => {
                            out.push(Diagnostic::error(
                                rule::BPMN_STRUCTURE,
                                id,
                                format!(
                                    "error '{eref}' needs an errorCode — boundary \
                                     matching is by code"
                                ),
                            ));
                        }
                        Some(code) => {
                            if let Some(first) =
                                error_claims.insert((host, code.to_string()), id.clone())
                            {
                                out.push(Diagnostic::error(
                                    rule::BPMN_STRUCTURE,
                                    id,
                                    format!(
                                        "'{first}' already catches error code '{code}' on \
                                         this activity — a second boundary for the same \
                                         code can never fire"
                                    ),
                                ));
                            }
                        }
                    },
                },
            }
        }
    }
}
