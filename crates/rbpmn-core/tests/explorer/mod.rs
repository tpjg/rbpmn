//! Explicit-state exploration of the real `step` (docs/stress-testing.md §7),
//! and the universal invariant set of §1.
//!
//! `step` is pure, total and deterministic, so the reachable state graph of a
//! model is a finite object we can enumerate exhaustively — a model checker
//! without a modeling language, a second implementation, or any drift risk.
//! BFS from the initial state, firing every pending stimulus at every state,
//! deduplicating on a canonical key, checking the invariant set everywhere.
//!
//! Scope, deliberately: this proves **state** invariants exhaustively and says
//! nothing about **traces**. The canonical key collapses states that differ
//! only in id numbering and closed-work-item history — that is what makes
//! loops terminate — so two paths reaching the same state are indistinguishable
//! here. Confluence, golden traces and "exactly the winner's end event" belong
//! to `scenarios.rs` and `properties.rs`. The two are complements.

// Shared by several test binaries; each uses a different part.
#![allow(dead_code)]

use rbpmn_core::*;
use rbpmn_model::condition::{Expr, Literal};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet, VecDeque};

/// Backstop against an un-bounded exploration; no real model comes close.
pub const MAX_STATES: usize = 200_000;

// ---------------------------------------------------------------- invariants

/// The invariant set of docs/stress-testing.md §1, in the subset expressible
/// on core state alone. Checked at *every* reachable state.
pub fn check(proc: &ExecutableProcess, s: &InstanceState) -> Result<(), String> {
    // A token awaiting a decision is a pending stimulus too: `stimuli()`
    // offers `CompleteDecision` for it, and the engine answers it inside the
    // same transaction. Counting it is what keeps "deadlock" meaning "nothing
    // can ever move this" rather than "nothing this list knows about".
    let awaiting_decision = s
        .tokens()
        .filter(|(_, t)| t.wait == WaitKind::Decision)
        .count();
    let stimuli = s.open_work_items().count()
        + s.timers().count()
        + s.subscriptions().count()
        + awaiting_decision;
    if s.status == InstanceStatus::Active && stimuli == 0 {
        return Err("active instance with no pending stimulus (deadlock)".into());
    }

    // Terminal states hold no runtime state at all.
    if matches!(
        s.status,
        InstanceStatus::Completed | InstanceStatus::Terminated
    ) && (s.tokens().count() > 0
        || s.open_work_items().count() > 0
        || s.timers().count() > 0
        || s.subscriptions().count() > 0)
    {
        return Err(format!("terminal status {:?} left runtime state", s.status));
    }

    // Every parked token agrees with the thing it is parked behind.
    for (id, t) in s.tokens() {
        match &t.wait {
            WaitKind::WorkItem(w) => {
                let Some((_, item)) = s.work_items().find(|(i, _)| *i == *w) else {
                    return Err(format!("token {id:?} references missing work item {w:?}"));
                };
                if !item.open || item.token != id || item.element != t.node {
                    return Err(format!("token {id:?} and work item {w:?} disagree"));
                }
            }
            WaitKind::Timer(ti) => match s.timers().find(|(i, _)| *i == *ti) {
                Some((_, timer)) if timer.token == id => {}
                Some(_) => return Err(format!("timer {ti:?} points at another token")),
                None => return Err(format!("token {id:?} references missing timer {ti:?}")),
            },
            WaitKind::Scope(sc) => match s.scopes().find(|(i, _)| *i == *sc) {
                Some((_, scope)) if scope.token == id && scope.element == t.node => {}
                Some(_) => return Err(format!("scope {sc:?} disagrees with token {id:?}")),
                None => return Err(format!("token {id:?} references missing scope {sc:?}")),
            },
            WaitKind::Message(si) => match s.subscriptions().find(|(i, _)| *i == *si) {
                Some((_, sub)) if sub.token == id => {}
                Some(_) => return Err(format!("subscription {si:?} points at another token")),
                None => {
                    return Err(format!(
                        "token {id:?} references missing subscription {si:?}"
                    ));
                }
            },
            // `Decision` needs no cross-check: unlike a work item or a
            // timer, it is not backed by a row that could disagree with the
            // token. What answers it is a command, and `stimuli` supplies
            // one — so the explorer walks decision paths rather than
            // reporting them as deadlocks.
            WaitKind::Join { .. }
            | WaitKind::EventGateway
            | WaitKind::Incident
            | WaitKind::Decision => {}
        }
    }

    // Join arity — the local-counting precondition, stated as a property of
    // the state rather than as a transition guard.
    let mut per_join: BTreeMap<NodeIx, Vec<FlowIx>> = BTreeMap::new();
    for (_, t) in s.tokens() {
        if let WaitKind::Join { arrived_via } = &t.wait {
            per_join.entry(t.node).or_default().push(*arrived_via);
        }
    }
    for (node, mut flows) in per_join {
        let incoming = proc.node(node).incoming.len();
        if flows.len() > incoming {
            return Err(format!(
                "join '{}' holds {} tokens for {incoming} incoming flows",
                proc.node_id(node),
                flows.len()
            ));
        }
        flows.sort();
        let before = flows.len();
        flows.dedup();
        if flows.len() != before {
            return Err(format!(
                "join '{}' holds two tokens arrived via the same flow",
                proc.node_id(node)
            ));
        }
    }

    // No arm outlives the token it is armed on.
    for (id, timer) in s.timers() {
        if !s.tokens().any(|(t, _)| t == timer.token) {
            return Err(format!("timer {id:?} is armed on a dead token"));
        }
    }
    for (id, sub) in s.subscriptions() {
        if !s.tokens().any(|(t, _)| t == sub.token) {
            return Err(format!("subscription {id:?} is armed on a dead token"));
        }
    }

    // The uniform incident freeze: at least one token parked where it
    // failed. NOT exactly one — `Advancer::freeze` deliberately parks every
    // sibling that was still in flight as `Incident` too, so that a frozen
    // parallel branch is not silently lost (token conservation survives the
    // freeze, which is what makes a future repair API possible).
    if s.status == InstanceStatus::Failed {
        let incidents = s
            .tokens()
            .filter(|(_, t)| matches!(t.wait, WaitKind::Incident))
            .count();
        if incidents == 0 {
            return Err("failed instance has no incident token".to_string());
        }
    }

    // Scopes: nothing left behind, and every open scope is answered by the
    // token parked on it (mirrors the SQL fsck's scope predicates).
    if matches!(
        s.status,
        InstanceStatus::Completed | InstanceStatus::Terminated
    ) && s.scopes().count() > 0
    {
        return Err(format!("terminal status {:?} left open scopes", s.status));
    }
    for (id, scope) in s.scopes() {
        match s.tokens().find(|(t, _)| *t == scope.token) {
            Some((_, t)) if t.wait == WaitKind::Scope(id) && t.node == scope.element => {}
            Some(_) => return Err(format!("scope {id:?}'s parked token disagrees with it")),
            None => return Err(format!("scope {id:?} has no parked parent token")),
        }
        if scope.parent != rbpmn_core::ScopeId::ROOT
            && !s.scopes().any(|(other, _)| other == scope.parent)
        {
            return Err(format!("scope {id:?} has a missing parent scope"));
        }
    }
    Ok(())
}

// -------------------------------------------------------------- canonical key

/// Two states share a key iff they have identical future behaviour modulo id
/// renaming and closed-work-item history.
///
/// The monotonic counters (`next_token`, …) must NOT contribute: states that
/// are semantically identical but reached by different paths differ in them,
/// so keying on the raw state would make every loop explore forever. The key
/// is built from structure instead, with every id replaced by its referent.
pub fn canonical(proc: &ExecutableProcess, s: &InstanceState) -> String {
    let mut arms: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    for (_, t) in s.timers() {
        arms.entry(t.token.0)
            .or_default()
            .push(format!("T:{}:{}", proc.node_id(t.element), t.due));
    }
    for (_, sub) in s.subscriptions() {
        arms.entry(sub.token.0).or_default().push(format!(
            "M:{}:{}:{}",
            proc.node_id(sub.element),
            sub.message,
            sub.key
        ));
    }
    for a in arms.values_mut() {
        a.sort();
    }

    let mut tokens: Vec<String> = Vec::new();
    for (id, t) in s.tokens() {
        let wait = match &t.wait {
            WaitKind::Join { arrived_via } => format!("join@{}", proc.flow(*arrived_via).id),
            WaitKind::WorkItem(w) => {
                let item = s.work_items().find(|(i, _)| *i == *w).map(|(_, i)| i);
                match item {
                    Some(i) => format!("work:{}:{}", i.kind, i.topic),
                    None => "work:dangling".into(),
                }
            }
            WaitKind::Timer(_) => "timer".into(),
            WaitKind::Message(_) => "message".into(),
            WaitKind::EventGateway => "gateway".into(),
            WaitKind::Incident => "incident".into(),
            WaitKind::Decision => "decision".into(),
            WaitKind::Scope(_) => "subprocess".into(),
        };
        let armed = arms.get(&id.0).map(|a| a.join(",")).unwrap_or_default();
        // The scope's *depth* rather than its id: ids grow with every
        // subprocess entry, so including them would make every loop
        // iteration a fresh state and the exploration would never converge.
        let mut depth = 0;
        let mut at = t.scope;
        while let Some((_, sc)) = s.scopes().find(|(i, _)| *i == at) {
            depth += 1;
            at = sc.parent;
        }
        tokens.push(format!(
            "{}|{}|d{}|[{}]",
            proc.node_id(t.node),
            wait,
            depth,
            armed
        ));
    }
    tokens.sort();
    format!("{:?}|{}|{}", s.status, s.variables, tokens.join(";"))
}

// ------------------------------------------------------------------ stimuli

/// Walk every node reachable from the start, boundary events included, and
/// collect the conditions on their outgoing flows.
pub fn reachable_conditions(proc: &ExecutableProcess, codes: &[String]) -> Vec<Expr> {
    let mut seen = HashSet::new();
    // Every scope's start is a root: no sequence flow connects a subprocess
    // to its body, so walking flows alone never enters one — the alphabet
    // would come back empty for every hierarchical model and the
    // exploration would silently never drive a gateway inside a subprocess.
    let mut queue: VecDeque<NodeIx> = (0..).map_while(|s| proc.try_scope_start(s)).collect();
    let mut found = Vec::new();
    while let Some(n) = queue.pop_front() {
        if !seen.insert(n) {
            continue;
        }
        for &f in &proc.node(n).outgoing {
            if let Some(c) = &proc.flow(f).condition {
                found.push(c.clone());
            }
            queue.push_back(proc.flow(f).target);
        }
        for &b in proc.timer_boundaries(n) {
            queue.push_back(b);
        }
        for code in codes {
            if let Some(b) = proc.error_boundary(n, code) {
                queue.push_back(b);
            }
        }
    }
    found
}

fn leaves(e: &Expr, out: &mut Vec<(Vec<String>, Literal)>) {
    match e {
        Expr::Cmp { path, value, .. } => out.push((path.clone(), value.clone())),
        Expr::And(parts) | Expr::Or(parts) => parts.iter().for_each(|p| leaves(p, out)),
    }
}

/// The finite patch alphabet §7 requires, derived from the model's own
/// conditions: for every comparison, patches landing on both sides of it.
/// Without a bound here the variable document grows without limit and the
/// state space is infinite.
pub fn patch_alphabet(proc: &ExecutableProcess, codes: &[String]) -> Vec<Value> {
    let mut lits = Vec::new();
    for e in reachable_conditions(proc, codes) {
        leaves(&e, &mut lits);
    }
    let mut out = vec![json!({})];
    for (path, lit) in lits {
        let candidates = match lit {
            Literal::Bool(b) => vec![json!(b), json!(!b)],
            // The literal itself and a neighbour either side, so the
            // explorer walks both sides of every threshold. `as_f64` is fine
            // here: these are probe values, not a comparison — the exactness
            // that matters lives in `condition::compare`.
            Literal::Num(n) => {
                let v = n.as_f64();
                vec![json!(v), json!(v - 1.0), json!(v + 1.0)]
            }
            Literal::Str(s) => vec![json!(s), json!("~other~")],
            Literal::Null => vec![Value::Null, json!("~other~")],
        };
        for c in candidates {
            out.push(path.iter().rev().fold(c, |acc, seg| json!({ seg: acc })));
        }
    }
    let mut seen = HashSet::new();
    out.retain(|v| seen.insert(v.to_string()));
    out
}

/// Every command the outside world could issue against this state.
pub fn stimuli(s: &InstanceState, patches: &[Value], codes: &[Option<String>]) -> Vec<Command> {
    let mut out = Vec::new();
    for (id, _) in s.open_work_items() {
        for p in patches {
            out.push(Command::CompleteWorkItem {
                id,
                patch: p.clone(),
            });
        }
        for c in codes {
            out.push(Command::RaiseError {
                id,
                code: c.clone(),
            });
        }
    }
    for (id, _) in s.timers() {
        out.push(Command::FireTimer { id });
    }
    for (id, _) in s.subscriptions() {
        for p in patches {
            out.push(Command::DeliverMessage {
                id,
                patch: p.clone(),
            });
        }
    }
    // A token parked on a decision is waiting for a command like any other.
    // The engine answers it inside the same transaction, so an *operator*
    // never sees this state — but the core does, between the two steps, and
    // that is exactly the level this explorer works at. Both answers are
    // supplied: a value, and the `None` that freezes, so the incident branch
    // is walked too.
    for (id, t) in s.tokens() {
        if t.wait == WaitKind::Decision {
            for p in patches {
                out.push(Command::CompleteDecision {
                    token: id,
                    answer: Some(p.clone()),
                    reason: None,
                });
            }
            out.push(Command::CompleteDecision {
                token: id,
                answer: None,
                reason: Some("explored".to_string()),
            });
        }
    }
    out
}

// ---------------------------------------------------------------- exploration

pub struct Report {
    pub states: usize,
    pub terminals: usize,
    pub violations: Vec<String>,
    /// Hit the state budget: the exploration is incomplete, not clean.
    pub capped: bool,
}

pub fn explore(proc: &ExecutableProcess, initial: Value, codes: &[String]) -> Report {
    let patches = patch_alphabet(proc, codes);
    let mut codes_opt: Vec<Option<String>> = codes.iter().cloned().map(Some).collect();
    codes_opt.push(None); // an unmatched failure: the incident path

    let mut state = InstanceState::new();
    step(proc, &mut state, Command::Start { variables: initial }).expect("start");

    let mut visited = HashSet::from([canonical(proc, &state)]);
    let mut frontier = VecDeque::from([state]);
    let mut r = Report {
        states: 1,
        terminals: 0,
        violations: Vec::new(),
        capped: false,
    };

    while let Some(s) = frontier.pop_front() {
        if let Err(v) = check(proc, &s) {
            r.violations.push(v);
            if r.violations.len() > 20 {
                return r;
            }
        }
        if s.status != InstanceStatus::Active {
            r.terminals += 1;
            continue;
        }
        for cmd in stimuli(&s, &patches, &codes_opt) {
            let mut next = s.clone();
            match step(proc, &mut next, cmd.clone()) {
                Ok(_) => {}
                // A typed refusal is the engine correctly declining an illegal
                // stimulus — not a finding. `Invariant` is one by definition:
                // lint-clean models cannot reach it.
                Err(StepError::Invariant(m)) => {
                    r.violations
                        .push(format!("StepError::Invariant on {cmd:?}: {m}"));
                    continue;
                }
                Err(_) => continue,
            }
            if visited.insert(canonical(proc, &next)) {
                r.states += 1;
                if r.states >= MAX_STATES {
                    r.capped = true;
                    return r;
                }
                frontier.push_back(next);
            }
        }
    }
    r
}

pub fn assert_clean(
    label: &str,
    proc: &ExecutableProcess,
    initial: Value,
    codes: &[String],
) -> usize {
    let r = explore(proc, initial, codes);
    assert!(
        !r.capped,
        "{label}: state space exceeded {MAX_STATES} — exploration was incomplete"
    );
    assert!(
        r.violations.is_empty(),
        "{label}: {} invariant violation(s) across {} states:\n  {}",
        r.violations.len(),
        r.states,
        r.violations.join("\n  ")
    );
    assert!(
        r.terminals > 0,
        "{label}: explored {} states but reached no terminal state",
        r.states
    );
    r.states
}
