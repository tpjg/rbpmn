//! The `balanced-gateways` region analysis — the rule that makes local token
//! counting at parallel joins provably correct.
//!
//! For every parallel split S we find its matching parallel join J and verify
//! the block structure between them:
//!
//!   * the branches of S are node-disjoint until J (no cross-branch merges),
//!   * no sequence flow enters the region from outside (tokens only enter
//!     through S),
//!   * every branch delivers exactly one token to J (exactly one edge from
//!     the branch into J; XOR arms hitting J separately would deadlock it),
//!   * no plain end event inside the region (it would consume a branch token
//!     and starve J; terminate end events are allowed — they cancel the whole
//!     instance, which is a legitimate escape),
//!   * loops may only wrap the whole block (a flow from inside the region
//!     back into S is rejected).
//!
//! **Interrupting** boundary events participate through host->boundary
//! pseudo-edges ([`Graph::region_succs`]): such a boundary continues its
//! host's token, so its path is part of the host's branch and must merge back
//! before J (or terminate) like any other path.
//!
//! A **non-interrupting** boundary is walked by nothing here, deliberately. It
//! spawns a *sibling* token rather than continuing the host's, so its path is
//! not part of any branch, delivers nothing to J, and its end events are not
//! "a plain end inside the region" — reading it as a branch would count a
//! token the branch never carries. What keeps that sound is
//! `boundary-side-path`: a side path is disjoint from everything else in the
//! scope and ends on its own. It is an *error*, and this analysis runs only on
//! an error-free scope, so it may assume the disjointness rather than re-check
//! it.
//!
//! This runs only on scopes with no other errors, so it may assume: gateways
//! are pure splits or joins, all flows resolve, every node reaches an end,
//! and every non-interrupting boundary path is a disjoint side path.

use super::structure::Graph;
use crate::diagnostics::{Diagnostic, rule};
use crate::model::{EndKind, NodeKind};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

pub fn check(g: &Graph, out: &mut Vec<Diagnostic>) {
    let n = g.scope.nodes.len();
    let splits: Vec<usize> = (0..n)
        .filter(|&i| matches!(g.node(i).kind, NodeKind::ParallelGateway) && g.out_deg(i) > 1)
        .collect();
    let joins: Vec<usize> = (0..n)
        .filter(|&i| matches!(g.node(i).kind, NodeKind::ParallelGateway) && g.in_deg(i) > 1)
        .collect();

    let mut claimed: BTreeMap<usize, usize> = BTreeMap::new();
    // Joins that were candidates of a split whose region check failed: the
    // split's diagnostics already explain the problem, so don't pile an
    // "unmatched join" error on top.
    let mut implicated: BTreeSet<usize> = BTreeSet::new();

    for &s in &splits {
        let candidates = join_candidates(g, s, &joins);
        let mut matched = None;
        let mut nearest_failure: Option<RegionCheck> = None;

        for &j in &candidates {
            let result = check_region(g, s, j);
            if result.errors.is_empty() {
                matched = Some((j, result));
                break;
            }
            if nearest_failure.is_none() {
                nearest_failure = Some(result);
            }
        }

        match matched {
            Some((j, result)) => {
                out.extend(result.warns);
                if claimed.insert(j, s).is_some() {
                    out.push(Diagnostic::error(
                        rule::BALANCED_GATEWAYS,
                        &g.node(j).id,
                        "parallel join matches more than one parallel split",
                    ));
                }
            }
            None => {
                implicated.extend(&candidates);
                match nearest_failure {
                    Some(result) => {
                        out.extend(result.errors);
                        out.extend(result.warns);
                    }
                    None => {
                        out.push(Diagnostic::error(
                            rule::BALANCED_GATEWAYS,
                            &g.node(s).id,
                            "parallel split has no matching parallel join — every split \
                             must be closed by a join so token counting stays local \
                             (balanced block structure)",
                        ));
                    }
                }
            }
        }
    }

    for &j in &joins {
        if !claimed.contains_key(&j) && !implicated.contains(&j) {
            out.push(Diagnostic::error(
                rule::BALANCED_GATEWAYS,
                &g.node(j).id,
                "parallel join has no matching parallel split",
            ));
        }
    }
}

/// Parallel joins reachable from `s`, nearest first (BFS order): the nearest
/// candidate is the modeler's most likely intended match, so its failure
/// diagnostics are the ones reported.
fn join_candidates(g: &Graph, s: usize, joins: &[usize]) -> Vec<usize> {
    let mut seen = vec![false; g.scope.nodes.len()];
    let mut order = Vec::new();
    let mut queue = VecDeque::from([s]);
    seen[s] = true;
    while let Some(v) = queue.pop_front() {
        for w in g.region_succs(v) {
            if !seen[w] {
                seen[w] = true;
                if joins.contains(&w) {
                    order.push(w);
                }
                queue.push_back(w);
            }
        }
    }
    order
}

struct RegionCheck {
    errors: Vec<Diagnostic>,
    warns: Vec<Diagnostic>,
}

fn check_region(g: &Graph, s: usize, j: usize) -> RegionCheck {
    let n = g.scope.nodes.len();
    let mut errors = Vec::new();
    let mut warns = Vec::new();

    let split_id = &g.node(s).id;
    let join_id = &g.node(j).id;

    // Phase 1: BFS each branch, assigning nodes to branches. J is a barrier;
    // S is never expanded. A node reached from two branches breaks the
    // disjointness that local token counting depends on.
    let branch_flows = &g.flow_out[s];
    let n_branches = branch_flows.len();
    let mut branch: Vec<Option<usize>> = vec![None; n];
    let mut cross_reported = vec![false; n];

    for (bi, &entry_flow) in branch_flows.iter().enumerate() {
        let entry = g.tgt(entry_flow);
        if entry == j || entry == s {
            continue;
        }
        let mut queue = VecDeque::from([entry]);
        while let Some(v) = queue.pop_front() {
            if v == j || v == s {
                continue;
            }
            match branch[v] {
                None => {
                    branch[v] = Some(bi);
                    for w in g.region_succs(v) {
                        queue.push_back(w);
                    }
                }
                Some(b) if b == bi => {}
                Some(_) => {
                    if !cross_reported[v] {
                        cross_reported[v] = true;
                        let node = g.node(v);
                        errors.push(Diagnostic::error(
                            rule::BALANCED_GATEWAYS,
                            &node.id,
                            format!(
                                "'{}' is reachable from more than one branch of parallel \
                                 split '{split_id}' — branches must stay disjoint until \
                                 the join '{join_id}'",
                                node.id
                            ),
                        ));
                        if !node.kind.is_gateway() && g.in_deg(v) > 1 {
                            warns.push(Diagnostic::warn(
                                rule::IMPLICIT_MERGE_AFTER_PARALLEL,
                                &node.id,
                                format!(
                                    "'{}' merges concurrent tokens implicitly — the spec \
                                     treats this as an uncontrolled merge (every arriving \
                                     token starts the activity again), not a join: the \
                                     classic 'task runs twice' trap. Use a parallel join.",
                                    node.id
                                ),
                            ));
                        }
                    }
                }
            }
        }
    }

    if !errors.is_empty() {
        // With overlapping branches the region is not well defined; further
        // checks would only produce noise.
        return RegionCheck { errors, warns };
    }

    let in_region = |v: usize| branch[v].is_some();

    // Phase 2: closure and content checks over the region.
    let mut branch_exits = vec![0usize; n_branches];
    for (v, assigned) in branch.iter().enumerate() {
        let Some(bi) = *assigned else { continue };
        for &fi in &g.flow_in[v] {
            let u = g.src(fi);
            if u != s && !in_region(u) {
                errors.push(Diagnostic::error(
                    rule::BALANCED_GATEWAYS,
                    &g.flow(fi).id,
                    format!(
                        "sequence flow enters the parallel region of split '{split_id}' \
                         from outside (into '{}') — tokens may only enter a parallel \
                         block through its split",
                        g.node(v).id
                    ),
                ));
            }
        }
        for &fi in &g.flow_out[v] {
            if g.tgt(fi) == j {
                branch_exits[bi] += 1;
            }
        }
        if let NodeKind::End(kind) = &g.node(v).kind
            && !matches!(kind, EndKind::Terminate)
        {
            errors.push(Diagnostic::error(
                rule::BALANCED_GATEWAYS,
                &g.node(v).id,
                format!(
                    "end event inside the parallel block of split '{split_id}' \
                         consumes a branch token, so join '{join_id}' can never fire — \
                         route the branch to the join, or use a terminate end event to \
                         cancel the whole instance"
                ),
            ));
        }
    }

    // Loops must wrap the whole block: a flow from inside back into S would
    // re-trigger the split with a single branch's token.
    for &fi in &g.flow_in[s] {
        if in_region(g.src(fi)) {
            errors.push(Diagnostic::error(
                rule::BALANCED_GATEWAYS,
                &g.flow(fi).id,
                format!(
                    "sequence flow loops from inside the parallel block back into its \
                     split '{split_id}' — loops must wrap the whole split/join block"
                ),
            ));
        }
    }

    // Every incoming edge of J must come from the region (or S directly).
    for &fi in &g.flow_in[j] {
        let u = g.src(fi);
        if u != s && !in_region(u) {
            errors.push(Diagnostic::error(
                rule::BALANCED_GATEWAYS,
                &g.flow(fi).id,
                format!(
                    "parallel join '{join_id}' receives a token from outside the region \
                     of its split '{split_id}'"
                ),
            ));
        }
    }

    // Each branch delivers exactly one token to J. Direct S->J edges are
    // empty branches delivering theirs directly.
    for (bi, &entry_flow) in branch_flows.iter().enumerate() {
        if g.tgt(entry_flow) == j {
            branch_exits[bi] += 1;
        }
    }
    for (bi, &exits) in branch_exits.iter().enumerate() {
        if exits != 1 {
            let entry = g.tgt(branch_flows[bi]);
            errors.push(Diagnostic::error(
                rule::BALANCED_GATEWAYS,
                split_id,
                format!(
                    "branch of parallel split '{split_id}' entered at '{}' delivers \
                     {exits} token(s) to join '{join_id}' — each branch must deliver \
                     exactly one (exclusive arms reaching the join on separate flows \
                     would deadlock it)",
                    g.node(entry).id
                ),
            ));
        }
    }

    RegionCheck { errors, warns }
}
