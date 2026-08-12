//! Explicit-state exploration of the real `step` (docs/stress-testing.md §7).
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

use rbpmn_core::*;
use rbpmn_model::condition::{Expr, Literal};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

/// Backstop against an un-bounded exploration; no real model comes close.
const MAX_STATES: usize = 200_000;

// ---------------------------------------------------------------- invariants

/// The invariant set of docs/stress-testing.md §1, in the subset expressible
/// on core state alone. Checked at *every* reachable state.
fn check(proc: &ExecutableProcess, s: &InstanceState) -> Result<(), String> {
    let stimuli = s.open_work_items().count() + s.timers().count() + s.subscriptions().count();
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
            WaitKind::Message(si) => match s.subscriptions().find(|(i, _)| *i == *si) {
                Some((_, sub)) if sub.token == id => {}
                Some(_) => return Err(format!("subscription {si:?} points at another token")),
                None => {
                    return Err(format!(
                        "token {id:?} references missing subscription {si:?}"
                    ));
                }
            },
            WaitKind::Join { .. } | WaitKind::EventGateway | WaitKind::Incident => {}
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

    // The uniform incident freeze: exactly one token, parked where it failed.
    if s.status == InstanceStatus::Failed {
        let incidents = s
            .tokens()
            .filter(|(_, t)| matches!(t.wait, WaitKind::Incident))
            .count();
        if incidents != 1 {
            return Err(format!("failed instance has {incidents} incident tokens"));
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
fn canonical(proc: &ExecutableProcess, s: &InstanceState) -> String {
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
        };
        let armed = arms.get(&id.0).map(|a| a.join(",")).unwrap_or_default();
        tokens.push(format!("{}|{}|[{}]", proc.node_id(t.node), wait, armed));
    }
    tokens.sort();
    format!("{:?}|{}|{}", s.status, s.variables, tokens.join(";"))
}

// ------------------------------------------------------------------ stimuli

/// Walk every node reachable from the start, boundary events included, and
/// collect the conditions on their outgoing flows.
fn reachable_conditions(proc: &ExecutableProcess, codes: &[String]) -> Vec<Expr> {
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([proc.start()]);
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
fn patch_alphabet(proc: &ExecutableProcess, codes: &[String]) -> Vec<Value> {
    let mut lits = Vec::new();
    for e in reachable_conditions(proc, codes) {
        leaves(&e, &mut lits);
    }
    let mut out = vec![json!({})];
    for (path, lit) in lits {
        let candidates = match lit {
            Literal::Bool(b) => vec![json!(b), json!(!b)],
            Literal::Num(n) => vec![json!(n), json!(n - 1.0), json!(n + 1.0)],
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
fn stimuli(s: &InstanceState, patches: &[Value], codes: &[Option<String>]) -> Vec<Command> {
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
    out
}

// ---------------------------------------------------------------- exploration

struct Report {
    states: usize,
    terminals: usize,
    violations: Vec<String>,
}

fn explore(proc: &ExecutableProcess, initial: Value, codes: &[String]) -> Report {
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
                assert!(r.states < MAX_STATES, "state space exceeded {MAX_STATES}");
                frontier.push_back(next);
            }
        }
    }
    r
}

fn assert_clean(label: &str, proc: &ExecutableProcess, initial: Value, codes: &[String]) -> usize {
    let r = explore(proc, initial, codes);
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

// ------------------------------------------------------------------ fixtures

#[derive(Deserialize)]
struct Scenario {
    fixture: String,
    #[serde(default)]
    bindings: Bindings,
    #[serde(default)]
    variables: Value,
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../rbpmn-model/tests/fixtures")
}

/// Error codes the model declares — the alphabet for `RaiseError`.
fn declared_error_codes(xml: &str) -> Vec<String> {
    let mut codes = Vec::new();
    for part in xml.split("errorCode=\"").skip(1) {
        if let Some(end) = part.find('"') {
            let code = part[..end].to_string();
            if !codes.contains(&code) {
                codes.push(code);
            }
        }
    }
    codes
}

/// Every scenario's (fixture, bindings, variables) triple is a distinct
/// starting point worth exploring — including the ones that start an instance
/// into an immediate incident.
#[test]
fn corpus_state_spaces_hold_the_invariants() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("scenario directory")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no scenarios found in {}", dir.display());

    let mut seen = HashSet::new();
    let (mut explored, mut states) = (0usize, 0usize);
    for path in files {
        let sc: Scenario = serde_json::from_str(&fs::read_to_string(&path).unwrap())
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let key = format!(
            "{}|{}|{}",
            sc.fixture,
            serde_json::to_string(&sc.bindings).unwrap(),
            sc.variables
        );
        if !seen.insert(key) {
            continue;
        }

        let xml = fs::read_to_string(fixtures_dir().join(&sc.fixture)).unwrap();
        let defs = rbpmn_model::parse(&xml).unwrap();
        let proc = ExecutableProcess::compile(&defs, "p", &sc.bindings)
            .unwrap_or_else(|e| panic!("{}: {e}", sc.fixture));
        states += assert_clean(
            &sc.fixture,
            &proc,
            sc.variables.clone(),
            &declared_error_codes(&xml),
        );
        explored += 1;
    }
    assert!(explored > 0, "explored nothing");
    println!("corpus: {explored} starting points, {states} reachable states, all clean");
}

// ----------------------------------------------------------------- synthetic

/// A block-structured model wider than anything in the corpus: one parallel
/// split into `branches` branches of `depth` sequential user tasks, joined.
/// Concurrency is what makes state spaces grow, and the fixtures top out at
/// three branches.
fn parallel_block(branches: usize, depth: usize) -> String {
    let (mut nodes, mut flows) = (String::new(), String::new());
    let mut n = 0usize;
    let flow = |flows: &mut String, src: &str, tgt: &str, n: &mut usize| {
        *n += 1;
        let id = format!("f{n}");
        flows.push_str(&format!(
            "    <bpmn:sequenceFlow id=\"{id}\" sourceRef=\"{src}\" targetRef=\"{tgt}\" />\n"
        ));
        id
    };

    let first = flow(&mut flows, "start", "ps", &mut n);
    nodes.push_str(&format!(
        "    <bpmn:startEvent id=\"start\"><bpmn:outgoing>{first}</bpmn:outgoing></bpmn:startEvent>\n"
    ));

    let (mut split_out, mut join_in, mut tasks) = (Vec::new(), Vec::new(), String::new());
    for b in 0..branches {
        let mut incoming = flow(&mut flows, "ps", &format!("t{b}_0"), &mut n);
        split_out.push(incoming.clone());
        for i in 0..depth {
            let me = format!("t{b}_{i}");
            let target = if i + 1 == depth {
                "pj".to_string()
            } else {
                format!("t{b}_{}", i + 1)
            };
            let out = flow(&mut flows, &me, &target, &mut n);
            tasks.push_str(&format!(
                "    <bpmn:userTask id=\"{me}\"><bpmn:incoming>{incoming}</bpmn:incoming>\
                 <bpmn:outgoing>{out}</bpmn:outgoing></bpmn:userTask>\n"
            ));
            if i + 1 == depth {
                join_in.push(out.clone());
            }
            incoming = out;
        }
    }

    let tag = |name: &str, ids: &[String]| -> String {
        ids.iter()
            .map(|i| format!("<bpmn:{name}>{i}</bpmn:{name}>"))
            .collect()
    };
    nodes.push_str(&format!(
        "    <bpmn:parallelGateway id=\"ps\"><bpmn:incoming>{first}</bpmn:incoming>{}</bpmn:parallelGateway>\n",
        tag("outgoing", &split_out)
    ));
    nodes.push_str(&tasks);
    let last = flow(&mut flows, "pj", "end", &mut n);
    nodes.push_str(&format!(
        "    <bpmn:parallelGateway id=\"pj\">{}<bpmn:outgoing>{last}</bpmn:outgoing></bpmn:parallelGateway>\n",
        tag("incoming", &join_in)
    ));
    nodes.push_str(&format!(
        "    <bpmn:endEvent id=\"end\"><bpmn:incoming>{last}</bpmn:incoming></bpmn:endEvent>\n"
    ));

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <bpmn:definitions xmlns:bpmn=\"http://www.omg.org/spec/BPMN/20100524/MODEL\" \
         id=\"defs\" targetNamespace=\"https://rbpmn.dev/generated\">\n\
         \x20 <bpmn:process id=\"p\" isExecutable=\"true\">\n{nodes}{flows}  </bpmn:process>\n\
         </bpmn:definitions>\n"
    )
}

fn explore_parallel_block(branches: usize, depth: usize) -> usize {
    let xml = parallel_block(branches, depth);
    let defs = rbpmn_model::parse(&xml).expect("generated model parses");
    let proc = ExecutableProcess::compile(&defs, "p", &Bindings::default())
        .expect("generated model is block-structured and must lint clean");
    assert_clean(&format!("Par({branches}x{depth})"), &proc, json!({}), &[])
}

/// Widths and depths beyond what the fixtures reach, kept small enough that
/// the whole test stays in the tens of milliseconds. Cost is exponential in
/// branch width and only polynomial in depth, so depth is the cheap axis.
#[test]
fn synthetic_parallel_blocks_hold_the_invariants() {
    let mut total = 0;
    for (branches, depth) in [(2, 1), (3, 1), (4, 1), (2, 3), (3, 2), (4, 2), (3, 3)] {
        total += explore_parallel_block(branches, depth);
    }
    println!("synthetic: {total} reachable states, all clean");
}

/// Wider and deeper models — `cargo test -- --ignored`. Cheap today (~0.3s,
/// ~16k states); it is `#[ignore]`d as the place to widen when investigating,
/// since cost is exponential in branch width: past ~10 branches this runs for
/// seconds, past ~16 for minutes.
#[test]
#[ignore = "wider sweep: run explicitly with --ignored"]
fn synthetic_parallel_blocks_hold_the_invariants_deeply() {
    for (branches, depth) in [(5, 2), (6, 2), (4, 4), (5, 3), (2, 20), (3, 8)] {
        let states = explore_parallel_block(branches, depth);
        println!("Par({branches}x{depth}): {states} states");
    }
}
