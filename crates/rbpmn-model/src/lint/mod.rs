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
use crate::model::*;
use std::collections::BTreeMap;
use structure::{Graph, reach};

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
        side_path_rules(&g, out);

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
                             time (`Bindings::topic`; default topic = the element id), \
                             never in the XML",
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
                CatchTrigger::Timer(spec) => check_timer(id, spec, node.kind.executes_cycle(), out),
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
                match &b.trigger {
                    // Non-interrupting is accepted for timers and messages:
                    // both spawn a sibling token onto a side path
                    // (`boundary-side-path`) and leave the host alone. It is
                    // also the one place a repeating `timeCycle` executes —
                    // `executes_cycle` is the single predicate saying so, and
                    // the compiler's chokepoint asks the same one — because a
                    // repeat only makes sense where the first occurrence does
                    // not end the wait.
                    BoundaryTrigger::Timer(spec) => {
                        check_timer(id, spec, node.kind.executes_cycle(), out)
                    }
                    // An error boundary is interrupting by definition: the
                    // activity that raised the error has already ended, so
                    // there is nothing left to run beside the handler.
                    // BPMN 2.0 fixes `cancelActivity="true"` for it, and
                    // "keeps running" is not a thing a failed activity can
                    // do — so this is malformed BPMN, not a phase
                    // restriction.
                    BoundaryTrigger::Error { .. } => {
                        if !b.cancel_activity {
                            out.push(Diagnostic::error(
                                rule::BPMN_STRUCTURE,
                                id,
                                "error boundary events are always interrupting — the \
                                 activity that raised the error has already ended, so \
                                 there is no host left to keep running. Remove \
                                 cancelActivity=\"false\"",
                            ));
                        }
                    }
                    // A message boundary is a message element like any other:
                    // the XML says *which* message is caught here, and the
                    // correlation key is manifest data checked at L2 against
                    // this element's own id.
                    BoundaryTrigger::Message(message_ref) => {
                        check_message(defs, id, message_ref.as_deref(), out)
                    }
                    BoundaryTrigger::None => out.push(Diagnostic::error(
                        rule::BPMN_STRUCTURE,
                        id,
                        "boundary event requires an event definition (timer, error or message)",
                    )),
                    BoundaryTrigger::Unsupported { tag } => out.push(Diagnostic::error(
                        rule::NO_UNSUPPORTED_ELEMENT,
                        id,
                        format!(
                            "'{tag}' boundary events are not supported \
                             (v1: timer, error, message)"
                        ),
                    )),
                }
            }
            NodeKind::ReceiveTask { message_ref } => {
                check_message(defs, id, message_ref.as_deref(), out)
            }
            // A business rule task needs nothing from the model: which
            // decision it invokes and where the answer lands are manifest
            // data, checked by `decision-has-binding` at compile against the
            // deployment's bundled artifacts. The XML stays standard-namespace
            // and says only that a decision happens here.
            NodeKind::BusinessRuleTask
            | NodeKind::UserTask
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
fn check_timer(id: &str, spec: &TimerSpec, executes_cycle: bool, out: &mut Vec<Diagnostic>) {
    // A cycle is executed on a non-interrupting boundary and nowhere else: on
    // an intermediate catch or an interrupting boundary the first occurrence
    // ends the wait, and "fire once, drop the rest" is the silent
    // reinterpretation other engines ship and this one refuses. The caller
    // answers with `NodeKind::executes_cycle`, which is also what the
    // compiler's chokepoint asks — one predicate, two readers.
    if matches!(spec, TimerSpec::Cycle(_)) && !executes_cycle {
        out.push(Diagnostic::error(
            rule::NO_UNSUPPORTED_ELEMENT,
            id,
            "a repeating timer (timeCycle) is only executed on a non-interrupting \
             boundary event — here the first occurrence ends the wait, so write a \
             timeDuration or timeDate instead",
        ));
        return;
    }
    let Some((what, text, literal)) = spec.literal_check() else {
        out.push(Diagnostic::error(
            rule::TIMER_ISO8601,
            id,
            if executes_cycle {
                "timer event definition needs a timeDate, a timeDuration or a timeCycle"
            } else {
                // A cycle would be refused here anyway (the branch above), so
                // offering one as a repair would be a round trip.
                "timer event definition needs a timeDate or timeDuration"
            },
        ));
        return;
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
/// into the instance variables) is registered in code (`Bindings::correlation`)
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
            // The business rule task earns its own sentence: it was an
            // accepted host until the message-boundary round, and the reason
            // it stopped being one is not "unsupported" but "impossible" —
            // the decision is answered inside the transaction that parks the
            // token, so the arm is created and withdrawn in one step. Saying
            // only "cannot attach" would read as a phase restriction that
            // might lift later; it never will.
            let why = if matches!(host_kind, NodeKind::BusinessRuleTask) {
                "boundary events cannot attach to a business rule task — the decision is \
                 answered inside the transaction that starts it, so a boundary here is \
                 armed and cancelled in the same step and can never fire. Model the \
                 alternative outcome as a decision result and an exclusive gateway after \
                 the task"
                    .to_string()
            } else {
                format!(
                    "boundary events cannot attach to a {} — supported hosts: {}",
                    host_kind.describe(),
                    NodeKind::SUPPORTED_BOUNDARY_HOSTS
                )
            };
            out.push(Diagnostic::error(rule::BOUNDARY_ON_SUPPORTED_HOST, id, why));
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

/// `boundary-side-path`: a non-interrupting boundary's path must be a **side
/// path** — disjoint from everything else in the scope, ending at its own end
/// event.
///
/// Why it is an error and not a warning. An interrupting boundary *continues*
/// its host's token: whatever block structure proved about that token still
/// holds on the boundary path, which is why the pseudo-edge model works. A
/// non-interrupting one spawns a **second** token that entered through no
/// split, so nothing was ever proved about it. Let that token reach a
/// parallel join and the join collects two tokens on one incoming flow — the
/// `Invariant` the `side-path-into-join` fixture demonstrates. Let it merge
/// into the host's continuation and everything after the host runs once per
/// trigger *plus* once for the host: the "task runs twice" trap, silently.
///
/// The rule, from `docs/design/boundary-messages.md` §2.3: let `P` be the
/// nodes reachable from the boundary `B` over sequence flows, plus the
/// pseudo-edges of boundaries attached to activities already in `P`. Every
/// node in `P \ {B}` must have **all** its predecessors (flows and host
/// pseudo-edges) inside `P`. A plain end event in `P` is required — that is
/// where the side token is consumed — and a terminate end is allowed, because
/// "on the fifth reminder, cancel the whole thing" is a legitimate escape.
///
/// One diagnostic per boundary, on the boundary: the offending node is named
/// in the message, and a merge reported at every node downstream of it would
/// be the same fix repeated.
fn side_path_rules(g: &Graph, out: &mut Vec<Diagnostic>) {
    for b in 0..g.scope.nodes.len() {
        let NodeKind::Boundary(data) = &g.node(b).kind else {
            continue;
        };
        // An unresolvable `attachedToRef` is `bpmn-structure`'s to report;
        // without a host there is no "beside the host" to describe.
        let (false, Some(host)) = (data.cancel_activity, g.host_of[b]) else {
            continue;
        };
        let boundary_id = &g.node(b).id;
        let host_id = &g.node(host).id;

        // P: forward closure from B over flows and every boundary pseudo-edge
        // (a boundary on an activity of the side path belongs to it too) —
        // the same traversal connectivity uses, so the two cannot disagree.
        let in_path = reach(g.scope.nodes.len(), &[b], |v| g.succs(v));

        // Disjointness: nothing outside the side path may reach into it. `B`
        // itself is exempt and is the only exemption — its one predecessor is
        // the host pseudo-edge, which is how the side path starts.
        let intruder = (0..g.scope.nodes.len())
            .filter(|&v| in_path[v] && v != b)
            .find_map(|v| {
                g.preds(v)
                    .into_iter()
                    .find(|&u| !in_path[u])
                    .map(|u| (v, u))
            });
        if let Some((v, u)) = intruder {
            out.push(Diagnostic::error(
                rule::BOUNDARY_SIDE_PATH,
                boundary_id,
                format!(
                    "non-interrupting boundary '{boundary_id}' starts a side path, but \
                     '{}' on it is also reached from '{}' outside it. A side path runs \
                     a *second* token beside '{host_id}' and must end on its own: it \
                     cannot rejoin the flow after '{host_id}' (the rest of the process \
                     would run twice) and it cannot reach a parallel join (which would \
                     collect a second token on one incoming flow). If you want \
                     'remind, then wait again', use an interrupting boundary and a loop",
                    g.node(v).id,
                    g.node(u).id,
                ),
            ));
            continue;
        }

        // A side path is a *multi-token* region: the boundary can fire again
        // while an earlier side token is still on it, so every per-scope
        // singleton inside it collides. A parallel join counts one token per
        // incoming flow *per scope*, and both side tokens live in the host's
        // scope — the second activation's token trips the join's Invariant.
        // The model generator found it the day the production landed (59 of
        // 200 interleavings on the minimal shape). A subprocess mints a scope
        // per entry, which is why a block inside one is fine: its body is
        // another scope and is not in P.
        let parallel = (0..g.scope.nodes.len())
            .find(|&v| in_path[v] && v != b && matches!(g.node(v).kind, NodeKind::ParallelGateway));
        if let Some(pg) = parallel {
            out.push(Diagnostic::error(
                rule::BOUNDARY_SIDE_PATH,
                boundary_id,
                format!(
                    "non-interrupting boundary '{boundary_id}' can fire again while an \
                     earlier side token is still inside the parallel block at '{}': two \
                     activations' tokens would meet at its join, which counts one token \
                     per incoming flow per scope — and both run in '{host_id}''s scope. \
                     Wrap the block in an embedded subprocess, which gives each \
                     activation its own scope",
                    g.node(pg).id,
                ),
            ));
            continue;
        }

        // Message arms on the side path are armed once per activation, and
        // an earlier activation's arm may still be open. Unless the
        // activation changed the key — a delivery patch can — the second arm
        // is the duplicate-(message, key) freeze. Sometimes right, so a
        // warning with the consequence named; the freeze is the loud backstop.
        //
        // Including the arms inside an embedded subprocess on the path, at
        // any depth. A subprocess mints a scope per entry and that is what
        // makes a *parallel block* safe there — the repair this very rule
        // recommends — but a subscription is keyed by (message, key) across
        // the whole instance, so a scope of its own buys an arm nothing.
        // `lint_scope` reaches that body on its own, with no idea it sits on
        // a side path, which is why the walk happens from here.
        for v in (0..g.scope.nodes.len()).filter(|&v| in_path[v] && v != b) {
            let mut arms: Vec<&FlowNode> = Vec::new();
            if g.node(v).kind.is_message_arm() {
                arms.push(g.node(v));
            }
            if let NodeKind::SubProcess(sp) = &g.node(v).kind {
                message_arms_within(&sp.body, &mut arms);
            }
            for arm in arms {
                out.push(Diagnostic::warn(
                    rule::SIDE_PATH_MESSAGE_ARM,
                    &arm.id,
                    format!(
                        "'{}' is armed once per activation of non-interrupting boundary \
                         '{boundary_id}', and an earlier activation's arm may still be \
                         open: unless each activation changes its correlation key, the \
                         second arm freezes the instance (duplicate-subscription)",
                        arm.id
                    ),
                ));
            }
        }

        // The side token has to be consumed somewhere: a terminate end takes
        // the whole scope with it, so only a plain end ends the side path.
        let plain_end = (0..g.scope.nodes.len()).any(|v| {
            in_path[v]
                && matches!(&g.node(v).kind, NodeKind::End(k) if !matches!(k, EndKind::Terminate))
        });
        if !plain_end {
            out.push(Diagnostic::error(
                rule::BOUNDARY_SIDE_PATH,
                boundary_id,
                format!(
                    "non-interrupting boundary '{boundary_id}' starts a side path with no \
                     plain end event: the sibling token it spawns beside '{host_id}' has \
                     nowhere to be consumed, and the instance can never complete. End the \
                     path at its own end event (a terminate end is allowed, and cancels \
                     the whole scope)"
                ),
            ));
        }
    }
}

/// Every message arm inside a scope's bodies, at any depth — the arms
/// `side_path_rules` cannot see because they live one scope down from the
/// path it walks. Boundary arms included: a message boundary inside the body
/// is armed once per activation exactly like a catch is.
fn message_arms_within<'a>(scope: &'a FlowScope, out: &mut Vec<&'a FlowNode>) {
    for node in &scope.nodes {
        if node.kind.is_message_arm() {
            out.push(node);
        }
        if let NodeKind::SubProcess(sp) = &node.kind {
            message_arms_within(&sp.body, out);
        }
    }
}
