# rbpmn — stress, fuzz and bounded verification

A design for the testing tier above the fixture corpus: randomized and
exhaustive testing that hunts for the failure modes hand-written fixtures
structurally cannot reach. Read `bpmn-engine-design.md` first — this document
assumes its vocabulary (tokens, block structure, wait states, the projection)
and exists to attack the claims it makes.

Nothing here replaces the fixture corpus. Fixtures are the *specification*;
everything below is the *search* for places where the implementation and the
specification part ways.

## The frame: find the third outcome

The engine's thesis is one sentence:

> Every model has exactly two outcomes: **rejected at deploy with a specific
> rule id**, or **executed correctly**.

The job of every test in this document is to find a **third outcome**. There
are six, and each is a distinct target:

| # | Third outcome | Hunted by |
|---|---|---|
| 1 | Linter accepts → `StepError::Invariant` or panic | mutation fuzz (§3c) — *hunted, nothing found* |
| 2 | Linter accepts → stuck token, no pending stimulus | state-space exploration (§2, §7) |
| 3 | Runs, but differently in Postgres than in the core | replay verification (§4) — *hunted, nothing found* |
| 4 | Correct alone, wrong under concurrency | the storm (§4) + chaos (§5) — *hunted, nothing found* |
| 5 | Linter **rejects a valid model** (false positive) | model generator (§3) |
| 6 | Native and WASM lint disagree | parity fuzz (§6) |

Outcome 1 is the Camunda-lineage bug class the whole design exists to
prevent — a model that lints clean and then executes with wrong token
semantics. §3c is the hunt for it, and it currently comes up empty: the
linter refuses ~99% of structural mutations and the survivors are harmless.

Outcome 5 had no test at all until the generator landed — the `reject/`
fixtures prove the linter catches what it should, and nothing proved it
*doesn't* catch what it shouldn't. It is now covered for everything the block
grammar can express (§3a).

Two properties of this codebase make the search unusually cheap, and both
should be treated as load-bearing infrastructure, not conveniences:

- **`step` is pure, total and deterministic.** A failing random run is a
  seed that reproduces forever, shrinks, and converts directly into a
  fixture.
- **`Event`'s `Display` format is stable API.** Traces are comparable across
  the core, the database, and time — which is what makes differential
  testing possible at all.

---

## 1. Universal invariants

Every technique below checks the same invariant set. It lives as `check` in
`crates/rbpmn-core/tests/explorer/mod.rs`, shared by `explore.rs` and
`mutation.rs`; promote it into the crate behind
`#[cfg(any(test, feature = "invariants"))]` when a consumer outside the test
tree needs it (the fsck of §4). All of the below are implemented there except
**no zombie transitions**, which inspects the result of applying commands
rather than a single state, and so belongs to §2's driver:

- **No deadlock.** `status == Active` ⟹ at least one pending stimulus
  (open work item, armed timer, or open subscription).
- **Token conservation.** `tokens.count()` equals the sum of tokens in each
  `WaitKind`, every `Token.node` is a valid `NodeIx`, and every open work
  item is referenced by exactly one token (and vice versa).
- **Join arity.** For each parallel join, the count of tokens parked with
  `WaitKind::Join` never exceeds its `incoming.len()`, and no two carry the
  same `arrived_via`. This is the local-counting precondition made
  checkable — `step` already errors on the second arrival via one flow;
  the invariant states it as a state property rather than a transition
  guard.
- **Arm integrity.** Every `TimerState.token` and `SubscriptionState.token`
  points at a live token whose `WaitKind` is consistent with being armed
  (`Timer`, `Message`, `EventGateway`, or `WorkItem`/`Message` for
  boundaries).
- **Terminal freeze.** `Completed` / `Terminated` ⟹ zero tokens, zero open
  work items, zero timers, zero subscriptions. `Failed` ⟹ exactly the
  uniform incident shape (a token with `WaitKind::Incident`, no arms on
  that token).
- **No zombie transitions.** From any terminal status, every command
  returns a typed error and leaves the serialized state **byte-identical**.

The last one is worth stating separately because it is the cheapest strong
property in the entire document: serialize, apply every command in the
alphabet, assert `Err(_)` and unchanged bytes.

---

## 2. Tier 0 — generalize what already exists

`crates/rbpmn-core/tests/properties.rs` fixes a completion priority for three
fixtures. The general form costs a day and subsumes it.

**The driver.** At any quiescent state the pending stimuli are
`open_work_items ∪ timers ∪ subscriptions`. A driver picks one (by strategy),
supplies a patch from a small alphabet, steps, and checks §1. Strategies:

- `Random(seed)` — proptest-driven, over the whole `accept/` corpus.
- `Priority(order)` — the existing behavior, kept for readability.
- `Exhaustive` — see §7; the same driver with a visited set.

**Rehydration differential.** Half a day, and it is the crash-safety proof.
Run every trace twice: once holding `InstanceState` in memory, once forcing
`counters()` → `rehydrate()` between *every* step. Assert identical traces.
Today nothing tests rehydration from arbitrary mid-flight states — the golden
scenarios only rehydrate implicitly along happy paths. This is the property
that says *a crash between any two steps is invisible*.

---

## 3. Tier 1 — the model generator (the synthetic TCK)

Design brief, learning #3: *no execution conformance suite exists — our tests
ARE the assurance.* A generator is how that assurance stops being limited by
how many fixtures a human felt like writing.

Generate over the **block grammar the region analysis claims to enforce**:

```
Block ::= Task(kind, topic, writes)
        | Seq(Block, Block)
        | Xor(Block+, default)
        | Par(Block+)
        | Loop(Block)
        | WithBoundary(error | timer, Block, Block)
        | EventGateway((catch, Block)+)
```

Serialize to real BPMN XML (reuse `just fixtures-di` for layout, so every
generated model renders in the playground). **Bound `Par` width at around 6**
and let depth, sequence length and nesting grow freely — §7's measurements
show that keeps every generated model exhaustively verifiable, while wide
splits are what make the state space explode.

**Status: (a) and (b) are landed** as `crates/rbpmn-core/tests/modelgen/mod.rs`
(grammar, XML emitter, oracle, driver) and `tests/generator.rs` (the
properties). The grammar implemented is `Task | Seq | Xor | Par | Loop` —
enough to cover everything `balanced-gateways` is actually about. `Loop` is
emitted as the fixture-proven shape: exclusive join, body, a control task, an
exclusive split whose back-edge is conditional and whose exit is the default.

`Sub` was added when phase 6 made subprocesses executable. It is a *no-op* to
the oracle — `Sub(B)` executes exactly what `B` does — which is what makes it
sharp: any scope bookkeeping that leaks into execution shows up immediately as
a task count that no longer matches the plain block. The emitter nests each
subprocess's start, end, body and flows inside its element, so generated
models are real scope trees, and mutating one can now produce a cross-scope
flow — a rule the flat grammar could not reach at all.

Not yet generated, and why: `WithBoundary` and `EventGateway` need care that
the core grammar does not. In the accepted fixtures a boundary path runs its
handler and then reaches **its own end event** rather than rejoining the main
flow — so a boundary inside a parallel branch would put an end event in a
branch (`end-event-in-branch`, a reject fixture) and would starve the join if
it fired. They are therefore only legal in positions the generator would have
to reason about specially, and the oracle has to model "this token stops here"
rather than "this token continues". Worth doing; deliberately not bundled in
with the part that needed no special cases.

Four properties fall out:

**(a) The linter has no false positives.** Every generated model must lint
clean — no errors *and no warnings*: a warning on a machine-generated,
textbook-block-structured model would mean the rule fires on something it
should not. The generator is an *independent second implementation* of "what
block structure means"; any disagreement is a generator bug or a linter
over-reach, and both are worth knowing. This is the only test of outcome 5
that exists.

> **Result: 100 000 generated models, zero false positives** (re-run after
> phase 6 with `Sub` in the grammar; subprocesses, nested subprocesses, loops
> around scopes and scopes inside parallel branches all lint clean and execute
> as the oracle predicts). 50 000 on the
> narrow grammar (depth 4, width ≤3) and 50 000 on the wide one (depth 6,
> `Par` up to 6), each also driven to completion against the oracle. Every
> composition the grammar reaches — loops inside parallel branches, parallel
> blocks inside exclusive branches, nested loops — lints clean and executes as
> predicted. `balanced-gateways` is not over-strict on anything this grammar
> can express.

**(b) Structural oracle → differential execution.** The generator holds the
block tree, so a small recursive interpreter over that tree predicts — for a
chosen set of XOR decisions and loop counts — exactly which tasks execute and
how many times, *without running the engine*. Two independent implementations
of BPMN semantics, differentially tested. That is as close to a TCK as this
domain permits.

The oracle is fifteen lines, which is the point: it is small enough to audit
by eye, and `the_oracle_itself_is_right` pins it against hand-computed answers
so a bug in the oracle cannot silently excuse the engine. Decisions are shared
input to both sides — XOR choices ride in the initial variable document, loop
counts are enforced by the driver through each loop's control task — and the
driver picks which open work item to complete pseudo-randomly, so interleaving
varies while the predicted outcome must not.

For the oracle to be sharp, the generator must control data: give each task a
disjoint variable namespace when testing confluence, and a deliberately shared
one when testing merge-patch interaction.

**(c) Mutation fuzz — the linter-hole hunt.** Take a valid generated model and
apply one structural mutation: retarget a flow across a region boundary,
delete a join, add a second outgoing flow to a task, point a branch into a
sibling block, swap `parallelGateway` → `inclusiveGateway`, move an end event
inside a branch. Then assert the dichotomy holds:

```
linter rejects  → assert a specific rule id (never a generic error)
linter accepts  → execute it; ANY Invariant, panic, stuck token, or
                  oracle mismatch is a hole in balanced-gateways
```

A mutant that survives the linter *and* the executor is fine — the linter is
allowed to be permissive where semantics stay correct. A mutant that survives
the linter and breaks the executor is outcome 1.

**Landed** as `crates/rbpmn-core/tests/mutation.rs`, with seven mutations:
retarget a flow, re-source a flow, drop a flow, give a task a second outgoing
flow, swap a gateway's kind, turn a gateway inclusive, and starve a parallel
join. Accepted mutants are not merely "run once" — they get the full
explicit-state exploration of §7, so the invariant set judges them at every
reachable state. `CompileError::Internal` counts as a hazard by its own
wording ("lint should have prevented this"), and every rejection's rule id is
checked against the published `CATALOGUE`, since rule ids are stable API.

> **Result: zero linter holes.** Over a representative run, 567 of 572
> applicable mutants were **rejected** and 5 executed cleanly — the linter
> refuses ~99% of structural mutations, and the remainder are harmless.
> Verified non-vacuous the hard way: commenting out `regions::check` in the
> linter makes the fuzz report `LINTER HOLE via swap_gateway_kind` within a
> single run. The hunt works; there is currently nothing to find.

**(d) Prove the restriction earns its keep.** The inverse, and the most
interesting artifact: run *non*-block-structured models through the core with
the lint gate off, and see what actually goes wrong. The `reject/` fixtures
already are those models, so there is nothing to generate — they just need
executing. `ExecutableProcess::compile_without_lint` (feature
`unlinted-compile`, enabled only by this crate's own tests) is the gate
bypass.

The headline counterexample is the Camunda-lineage bug itself, reproduced on
demand by `without_block_structure_a_join_double_counts`:

> `cross-branch-merge` without the gate → `StepError::Invariant: second token
> arrived at join 'pj' via flow 'f7' — the linter's block structure guarantee
> is broken`

The full measured table, which the test pins so it cannot rot:

| Fixture | Rejected by | Without the gate |
|---|---|---|
| `cross-branch-merge` | `balanced-gateways` | **join double-counts** (`Invariant`) |
| `implicit-split` | `no-implicit-split` | **`Invariant`**: two outgoing flows |
| `orphan-parallel-join` | `balanced-gateways` | **deadlock** |
| `two-edges-into-join` | `balanced-gateways` | **deadlock** |
| `end-event-in-branch` | `balanced-gateways` | **deadlock** |
| `entry-into-region` | `balanced-gateways` | executes cleanly (18 states) |
| `parallel-missing-join` | `balanced-gateways` | executes cleanly (8 states) |

The last two rows are the honest result and worth stating plainly: block
structure is a **sufficient** condition that makes local join counting
provably correct, so an individual violation of it need not manifest a
hazard. Those two rules are conservative rather than hazard-driven. That is
not an argument for relaxing them — the theorem needs the whole property, not
the cases where breaking it happens to be survivable — but it is the
difference between a rule we can justify with a counterexample and one we
justify by the proof it enables.

**The fuzzer is a fixture factory.** Because `step` is deterministic, a
shrunk falsifying case is a `.bpmn` file plus a command sequence — i.e.
exactly a fixture and a scenario. Wire the harness to emit them into
`tests/fixtures/` and `tests/scenarios/` on failure. That closes the loop with
the *fixtures first* ground rule, and the failing model opens in the
playground with the token overlay, so a fuzz failure is something you can
**look at**.

Free side effect: generated models extend `just parity` from 52 fixtures to
however many you care to generate (§6).

---

## 3-bis. What a new phase costs this tier

Phase 6 (embedded subprocesses) is the first phase to land *after* this
document, and it is a useful measurement of how much of the tier adapts by
itself:

| Piece | Adapted how |
|---|---|
| Explicit-state exploration (§7) | **For free.** It iterates the scenario corpus, so the four new subprocess scenarios were picked up automatically (22 → 26 starting points, 145 → 172 states). |
| Replay verification (§4) | **For free.** Scopes added no new event kinds, so histories re-derive unchanged. |
| Invariant set / fsck (§1) | **Needed work.** The fsck was blind to `rbpmn_scope` entirely, and join arity has to group by `scope_no` now that joins count within a scope instance. |
| Storm and chaos (§4, §5) | **Needed work.** Their fixtures contained no subprocess, so the scope projection was never stressed, replayed or crashed — green, and meaningless for the newest code. |
| Model generator (§3) | **Needed work.** No `Sub` in the grammar meant no generated coverage of the newest semantics at all. |
| TLA+ specs (§7) | **Unaffected.** Scope rows are written under the same instance row lock, so the lock order the spec models is unchanged. |

The lesson worth carrying: **the parts keyed on a corpus adapt themselves; the
parts with a hand-written workload or grammar do not.** A green run after a
new phase is not evidence that the phase is covered — it is evidence that
nothing already covered broke. Check the workload actually reaches the new
code before believing the result. Both storm and chaos now assert they do.

## 4. Tier 2 — the concurrency storm, made to prove something

The naive storm ("N workers, M instances, did anything explode?") proves
little. Here is the version that proves a lot.

**Landed** as `crates/rbpmn-engine/tests/storm.rs`: the fsck, replay
verification, and a storm across three engines on separate connection pools.

> **Result: 60 instances, 1048 events, every history re-derived from the core;
> fsck clean, zero PostgreSQL deadlocks, no work item completed twice, no
> timer fired twice, no message delivered twice, and the tailing cursor
> delivered every event exactly once in `(txid, id)` order.** Scales on
> `RBPMN_STORM_ROUNDS` (300 instances / 5162 events in ~3 s).

**Replay verification.** After the storm, `rbpmn_event` holds the full ordered
history of every instance — and it is rich enough to *reconstruct the command
sequence* (`work-item-completed` + `variables-patched` → `CompleteWorkItem`,
`timer-fired` → `FireTimer`, `message-received` → `DeliverMessage`, …). So:
for each instance the storm ran, extract its stimuli, replay them through the
pure core, and assert the core's trace equals the database trace projected
onto core-visible kinds.

That single check converts a load test into a semantic conformance run over
every execution that happened. It is the systematic form of
`full_flow_matches_the_core_golden_trace`, which covered exactly one fixture —
and it directly tests the claim the architecture rests on: *the Postgres layer
is a projection of this core*. It is the only test of outcome 3.

Two details make it work without a hand-maintained list to drift. Engine-level
events (`work-item-retrying`, `timer-fire-failed`) are not `Event` variants, so
*failing to deserialize* is precisely the projection onto core-visible kinds.
And a `variables-patched` event always immediately follows the command that
carried the patch, so the merge patch is recoverable from the log — only the
initial variables of `Start` are not recorded, and the driver supplies those.

Verified non-vacuous by injecting a real projection bug: dropping `FlowTaken`
from what `persist_step` writes makes replay report the exact index where the
database trace and the core trace diverge.

The per-instance projection is well-defined precisely because of the phase-5
guarantee: all of an instance's events are written under its row lock, so
ascending `id` **is** the semantic order.

**Workload.** Several `Engine` instances on one database (active-active by
construction — there is no singleton to make this unfair), a mix of push-mode
workers, pull-mode task consumers, schedulers, `correlate` callers, and
terminates, all against overlapping instance sets. The mix matters more than
the volume: the point is to interleave *different code paths on the same
rows*.

**Global invariants**, checked from the event table alone:

- **Zero `40P01`.** The brief reasons explicitly about AB/BA lock ordering
  (timer claim vs. completion) and asserts the implemented ordering has no
  deadlock. Only a workload mixing completions, timer fires, correlations and
  terminates on the same instances across multiple nodes can falsify that.
- **Event-stream horizon under load.** Run a tailing reader with the
  safe-horizon cursor *during* the storm; diff its output against the final
  `SELECT … ORDER BY id`. Every event exactly once, per-instance ids
  ascending, nothing skipped. The xid8 horizon only breaks under many
  overlapping commits; `event_stream_never_misses_out_of_order_commits` is a
  crafted two-transaction case.
- **Exactly-once, counted globally.** Each join element started once per
  iteration; no work item completed twice; one `timer-fired` per
  `timer-armed`; successful `correlate` count equals consumed subscription
  count.
- **Zero orphans.** No token / work item / timer / subscription belonging to
  a non-active instance.

**`fsck` for rbpmn** — implemented as SQL queries rather than
"rehydrate and check", deliberately: that is what someone debugging a
production database would actually run, and it does not depend on the loader
being correct, which is part of what is under test. Factor these into
`check_invariants(pool) -> Vec<Violation>`, runnable against a live database
at any moment: every active instance's rows must rehydrate into a state that
satisfies §1. That is simultaneously the storm's assertion, the precondition
for chaos testing (§5), and a genuinely useful operator tool.

---

## 5. Tier 3 — chaos

Testing strategy #5 in the design brief — crash tests — had **no
implementation** from phase 2 until now. The transactional design's central
promise is "kill it anywhere, converge on restart", and nothing proved it.

**Landed** as `crates/rbpmn-engine/tests/chaos.rs`, on the same harness as the
storm. Faults injected while the workload runs:

- `pg_terminate_backend` on the node pools' backends (tagged by
  `application_name`, so the test's own control connection survives).
- Nodes dropped and rebuilt — pool and all — with work in flight.
- Handlers that fail (walking the retry budget into an incident) and panic
  outright, asserting the worker loop contains both.
- A 10 ms lease (the API floor) with consumers that deliberately outlive it,
  so peers reclaim underneath them continuously.

The assertion is what makes it evidence: chaos stops, the system gets a quiet
window to converge *on its own*, and then every instance must have drained,
the fsck must be clean, and **every instance's whole history must still
re-derive through the pure core**. Convergence is not "it kept running" — it
is "the history is exactly what an uninterrupted run would have produced".

> **Result: 240 instances, 4520 events replayed, 117 backends killed, 6 node
> restarts, 91 completions refused by an expired lease. All instances
> terminal (232 completed, 8 incidents from the failing handler), fsck clean,
> zero deadlocks, nothing applied twice.** Scales on `RBPMN_CHAOS_ROUNDS`.

> **What building it surfaced.** The first version never converged, and the
> reason is worth writing down: the default worker lease is **600 seconds**,
> so a node killed while holding one strands its work item for ten minutes.
> That is the documented design working correctly — "a crashed worker's items
> return within one TTL, no reaper needed" — but it means **recovery latency
> after a node dies is bounded by the lease TTL, not by anything the engine
> does**. Operators choosing a lease are choosing a crash-recovery window. The
> test now sets 250 ms and validates the claim instead of tripping over it.

**Not done: clock skew.** The brief claims node clocks never decide anything,
and the natural test — run one node with a skewed clock — is not reachable
in-process: the skew would have to be applied to the OS clock of a separate
process. What covers the substance today is `timer_fires_from_database_time`
and `date_timer_arms_at_the_absolute_instant` in `engine.rs`, which pin firing
to database time. A genuine skew test needs container-level isolation and is
left as a candidate.

---

## 6. Cheap side quests with outsized value

**FEEL differential against dsntk.** The highest confidence-per-day item in
this document. `condition::eval` claims FEEL-*exact* null semantics that
"must not change when dsntk swaps in" — null-safe equality, incomparable
types yielding null, Kleene `and`/`or`, root collapse. dsntk passes the DMN
TCK (3374/3391). Add `dsntk-feel` as a **dev-dependency only** (the
dependency-light rule for `rbpmn-model` is about the shipped crate), generate
random subset expressions × random variable documents, evaluate with both,
assert equal. This converts an aspiration verified by hand-written tests into
a verified fact *before* v1 ships, in exactly the corners where hand-written
tests get thin.

**Metamorphic equivalences.** No oracle needed, and they find real semantic
bugs:

- `Par` with one branch ≡ `Seq` of that branch, modulo gateway events.
- `Loop(B)` taken *n* times ≡ `Seq(B, …, B)` with *n* copies.
- Consistent renaming of all element ids ⟹ identical trace modulo renaming
  (catches accidental ordering-by-string-id dependencies).
- Permuting sequence-flow declaration order permutes the trace *exactly
  predictably* — the core promises branches spawn in declaration order, so
  this is a sharp test of the determinism claim.
- Once v2 lands: `Sub(B)` ≡ `B` modulo scope events — a strong test of the
  scope machinery, available the day subprocesses execute.

**Parity fuzz.** `just parity` guarantees native/WASM byte-identical lint
output over the fixture corpus. Point it at the generator's output and the
corpus becomes unbounded, for free.

**Confluence, stated correctly.** Confluence is *not* universal: the
event-gateway race and the boundary-timer race legitimately diverge by
stimulus order. The property must be quantified over *independent* stimuli —
which the generator knows structurally (distinct parallel branches, disjoint
variable namespaces). State it that way or it will produce false failures and
get weakened into uselessness.

---

## 7. Bounded verification: where Kani fits, and where it does not

Kani is a bounded model checker for Rust (CBMC backend): it explores symbolic
inputs exhaustively up to a bound and proves the absence of panics, overflow
and assertion failures. The question is which parts of this codebase it can
actually reach.

### It cannot verify `step`, and pretending otherwise costs weeks

`step` is the interesting theorem and the wrong target. Every one of these is
individually fatal to a CBMC encoding:

- `InstanceState` holds four `BTreeMap`s. `std::collections::BTreeMap` is
  built on raw pointers and manual memory management; symbolic reasoning
  about it is exactly the case CBMC handles worst.
- `serde_json::Value` is an unbounded recursive enum containing a map of
  `String`. Symbolic JSON is a non-starter.
- Every event carries an owned `String` (`proc.node_id(n).to_string()`), and
  every error path builds one with `format!`. Formatting machinery dominates
  the encoding.
- `Advancer::run` loops over a `VecDeque` whose length depends on graph
  shape and token count — an unwind bound that must be guessed, and that
  silently under-approximates when guessed low.

The honest verdict: **do not attempt Kani on `rbpmn-core`'s token
semantics.** The path people reach for — write a reduced, allocation-free
model over fixed-capacity arrays and verify *that* — buys a theorem about a
second implementation that can drift from the first, linked only by the
property tests you would have written anyway.

### For the token semantics, do explicit-state exploration on the real code

There is a strictly better option, and it is the single most valuable idea in
this document.

`step` is pure, total, deterministic, and `InstanceState` is `Clone`. That is
precisely the shape an explicit-state model checker needs. So write one — the
loop is thirty lines — that runs the **real production code**:

```
frontier = [ initial_state ]
visited  = { canonical(initial_state) }
while let Some(s) = frontier.pop_front():
    check_invariants(proc, &s)?                  // §1
    if s.status != Active: continue              // terminal: nothing to fire
    for cmd in pending_stimuli(&s) × patch_alphabet:
        let mut next = s.clone()
        match step(proc, &mut next, cmd):
            Err(Invariant(m)) => report(m),      // a real finding
            Err(_)            => continue,       // typed refusal: legal
            Ok(_)             => if visited.insert(canonical(&next)):
                                     frontier.push_back(next)
```

Note which errors mean what: a typed `StepError` is the engine correctly
refusing an illegal stimulus and is not a finding; `StepError::Invariant` is
by definition a bug, since lint-clean models cannot trigger it.

With a visited set this is a search over the **reachable state graph**, not a
walk over a path tree: it is TLC/SPIN semantics without a second
implementation, without a modeling language, and without a drift risk.
Combined with the generator (§3), "for every block-structured model up to
size *n*, every reachable state satisfies the invariants" becomes a nightly
job rather than an aspiration. That is bounded verification of the property
that matters, on the code that ships.

Three bounds are **required**, not optional — without them the state space is
infinite and the visited set never hits:

1. **Canonicalize before hashing.** `InstanceState` carries monotonic
   counters (`next_token`, `next_work_item`, …) and id-keyed maps. Two
   semantically identical states reached by different paths differ in those
   ids, so the raw serialization never repeats — a loop fixture would explore
   forever. Build the key from *structure* instead: status, variables, and
   the sorted set of `(element, wait descriptor, arms armed on this token)`
   with every id replaced by its referent.
2. **A finite patch alphabet.** Patches that introduce fresh keys grow the
   variable document without bound. Derive it from the model's own
   conditions: for each `Cmp { path, op, value }` on a reachable flow, emit
   patches setting `path` to values on both sides of the comparison. That is
   automatic, and it is exactly the alphabet needed to reach both branches of
   every XOR.
3. **A loop bound.** Cap iterations by construction — the generator emits
   loop conditions driven by a counter variable in the patch alphabet.

### Measured, not estimated

**Landed as `crates/rbpmn-core/tests/explore.rs`** — exactly the loop above,
against the real `step`. The existing corpus is trivial for it:

> **22 scenario starting points: 145 reachable states, <1 ms. Zero invariant
> violations, zero `StepError::Invariant`.** Plus synthetic parallel blocks
> wider than any fixture (702 states), and an `#[ignore]`d wider sweep.

That is the whole executable fixture corpus verified exhaustively, faster
than a single `cargo test` startup. The interesting question is how it scales,
so the same explorer was pointed at synthetic `Par(k branches × m sequential
tasks)` models (failure paths included — every task can also raise an
unmatched error):

| Model | Tasks | States | Time |
|---|---|---|---|
| `Par(4×1)` | 4 | 48 | 0 ms |
| `Par(4×2)` | 8 | 297 | 0 ms |
| `Par(6×2)` | 12 | 3 645 | 12 ms |
| `Par(4×4)` | 16 | 2 625 | 7 ms |
| `Par(8×2)` | 16 | 41 553 | 195 ms |
| `Par(3×16)` | 48 | 18 785 | 60 ms |
| `Par(6×4)` | 24 | 90 625 | 393 ms |
| `Par(2×40)` | 80 | 4 961 | 15 ms |
| `Par(10×2)` | 20 | 452 709 | 2.7 s |
| `Par(7×4)` | 28 | 515 625 | 2.7 s |
| `Par(16×1)` | 16 | 589 824 | 4.9 s |

Throughput is 120 000–165 000 states/second, and the shape of the curve is
the actionable part: **cost is exponential in concurrent branch width and
only polynomial in sequential depth.** `Par(2×40)` has eighty tasks and
finishes in 15 ms; `Par(16×1)` has sixteen and takes five seconds.

The design guidance follows directly: **the generator (§3) should bound
parallel width at around 6 and let depth, sequence length and nesting grow
freely.** That covers Method-and-Style models, which are hierarchical and
narrow at each level — precisely the shape this engine exists to serve — and
it keeps every generated model exhaustively verifiable in well under a
second.

### What state exploration does not cover

The canonical key deliberately collapses states that differ only in id
numbering and in *closed*-work-item history. That is what makes loops
terminate, and it is sound for **state** invariants — but it means two paths
reaching the same state become indistinguishable. In the prototype, all three
winners of `11-event-based-gateway`'s race collapse into a single `Completed`
state.

So: state exploration proves the invariants of §1 exhaustively; it says
nothing about **trace** properties. Confluence, "exactly the winner's end
event fired", event multiset equality and golden traces remain the job of the
path-based property tests in §2 and the scenario corpus. The two techniques
are complements, not substitutes.

### Where Kani genuinely earns its keep: the parsers

The criterion is sharp. Reach for a model checker where the **input space is
astronomically large and the code is allocation-light**; use exhaustive
enumeration where the domain is small enough to enumerate. By that test, the
targets are the string parsers, in this order:

**1. `iso8601::validate_datetime` / `validate_duration`.** Untrusted deploy
input, hand-rolled byte-level parsing, arithmetic that feeds a runtime
deadline (there is already a `reject/timer-overflow.bpmn` fixture — that
concern is real and known). Kani proves, for **all** inputs up to *N* bytes:
no panic, no arithmetic overflow, and — the good one — no out-of-bounds or
non-char-boundary string slice.

That last obligation is worth spelling out, because it is currently
discharged by an informal three-step argument. `validate_datetime` does
`s[*i..end]` with byte indices. It is safe today because (a) `digits` checks
`end <= b.len()` and that every byte in `*i..end` is an ASCII digit, (b)
`expect` only ever advances past ASCII bytes, and (c) the fraction loop only
advances over ASCII digits — so `i` is always a char boundary. That argument
is correct, and it is exactly the kind of thing a human should stop
maintaining by hand. Kani discharges it in seconds and keeps discharging it
after the next edit.

The harness is small enough to write now:

```rust
#[cfg(kani)]
mod proofs {
    use super::*;

    /// No panic, no overflow, no bad slice — for every ASCII input of
    /// length <= N. Raise N until verification time becomes unpleasant and
    /// record the bound reached in the harness name.
    #[kani::proof]
    #[kani::unwind(24)]
    fn validate_datetime_never_panics_len_16() {
        let bytes: [u8; 16] = kani::any();
        kani::assume(bytes.iter().all(|b| b.is_ascii() && *b != 0));
        let s = core::str::from_utf8(&bytes).unwrap();
        let _ = validate_datetime(s);   // the property IS reaching here
    }

    /// Validation is a pure predicate: no input is both accepted and
    /// rejected, and re-validating an accepted string still accepts.
    #[kani::proof]
    #[kani::unwind(24)]
    fn validate_duration_is_deterministic() {
        let bytes: [u8; 12] = kani::any();
        kani::assume(bytes.iter().all(|b| b.is_ascii() && *b != 0));
        let s = core::str::from_utf8(&bytes).unwrap();
        assert_eq!(validate_duration(s).is_ok(), validate_duration(s).is_ok());
    }
}
```

**2. `condition::parse` and `lex`.** Bounded symbolic input, panic-freedom,
plus the normalization property (`==` normalizes to `=`) and a
parse → display → parse round-trip.

**3. `condition::parse_qname`.** Tiny, self-contained, and shared between
conditions and correlation bindings. The right first harness: it will teach
you the toolchain in an afternoon.

**Do not use Kani for the FEEL evaluator's semantic laws.** The value domain
that matters is finite and tiny — `{Null, Bool×2, Num×3, Str×2}` × literals ×
6 operators, and `and`/`or` nesting to depth 2–3. That is a few thousand
combinations: a plain `#[test]` with nested loops gives the *same* theorem,
exhaustively, with no new tooling and no toolchain pin. Use the differential
against dsntk (§6) for the question that actually needs an oracle — "are
these semantics FEEL's?" — and exhaustive enumeration for "are these
semantics internally consistent?".

### Practical notes for the Kani harnesses

- Install with `cargo install --locked kani-verifier && cargo kani setup`.
  Kani pins its own nightly toolchain and does not disturb the workspace's
  stable 1.91. **Toolchain risk checked and cleared:** Kani 0.67.0 pins
  `nightly-2025-11-21`, which is *newer* than this workspace's 1.91 and well
  past edition 2024's stabilization in 1.85. Re-check on any Kani upgrade —
  Kani synchronizes with a specific nightly each monthly release, so this is
  a standing compatibility question rather than a settled one.
- Harnesses live behind `#[cfg(kani)]` inside `rbpmn-model`, so normal
  builds, the WASM build and the dependency-light rule are all untouched.
- Bounded symbolic strings are the standard idiom: an `[u8; N]` of
  `kani::any()`, `kani::assume` it is ASCII, then `str::from_utf8`. Start at
  N = 8 and raise it until verification time becomes unpleasant; report the
  bound reached in the harness name.
- The `format!` calls on error paths are the main cost driver. If a harness
  will not converge, add a `#[cfg(kani)]` error variant that returns `Err(())`
  instead of a formatted message — the proof is about control flow and
  arithmetic, not about message text.
- Every harness that verifies is a **regression barrier**: put them in CI as
  a separate, non-blocking job first, then promote once the runtime is known.

### What Kani cannot touch at all: the Postgres layer

Lock ordering, exactly-once timer firing across competing nodes, lease
transfer, the event horizon — none of it is reachable from Rust-level
symbolic execution, because the interesting behavior lives in Postgres's
concurrency semantics, not in the Rust.

If those claims deserve formal treatment, the right tool is **TLA+ on the
protocol, not the code**: model the locking paths and the lease as abstract
actions over an abstract store, and let TLC check deadlock-freedom and
exactly-once under all interleavings.

**Landed** in `spec/` (`just tla`, see `spec/README.md`). `LockOrder.tla`
models instance-row and item-row locking across N nodes; `Lease.tla` models
the work-item lease. Each ships with a companion config that is *expected to
fail*, so the checks are known to have teeth: `LockOrderHistorical.cfg`
restores the timer-claim order the design brief rejected and TLC reports
`Deadlock reached`, and `Lease_DoubleBelief.cfg` shows that two workers really
can both believe they hold one item — reachable, and harmless, because every
mutation is conditional on `lock_owner = $me AND lock_until > now()`.

> **Checking the model corrected a piece of the prose.** Deleting the
> NOWAIT/give-up action does *not* reintroduce the deadlock: with a single
> lock order there is no cycle to find, whoever waits. Deadlock freedom comes
> from the **order**; `NOWAIT` buys the separate thing `scheduler.rs`
> describes — not parking the drain loop behind an embedder's long-running
> `*_in_tx` transaction. Safety and throughput, two mechanisms, easy to
> conflate in prose and impossible to conflate in a model.

Still unmodelled and named as candidates: the event-stream safe horizon (xid8
visibility) and the deploy/undeclare race.

---

## 8. What all of this proves — and what it does not

Randomized testing **falsifies**; it does not prove. Stated precisely:

- The **differential** techniques (structural oracle, replay verification,
  dsntk) do not depend on guessing the right invariants in advance. An
  independent oracle catches bugs nobody thought to look for. This is where
  the confidence comes from.
- **Explicit-state exploration** (§7) *does* prove — but bounded twice over:
  "for this model, every reachable state satisfies the invariants of §1" is a
  complete statement about a finite state space, not about all models, and
  not about traces. Over a generated model population it is an extremely
  strong empirical shadow of the block-structure theorem, and nothing more.
  It has already been run over the existing corpus: 128 states, clean.
- **Kani** proves unbounded-over-inputs-up-to-*N* panic and overflow freedom
  for the parsers. Real proof, narrow scope.
- The storm proves nothing on its own. It becomes evidence only through
  replay verification and the fsck — assertions, not absence of crashes.
- Nothing here proves the *theorem* that local counting is correct under
  block structure. That remains a paper argument, supported by (d)'s
  counterexamples showing the restriction is load-bearing.

## 9. Suggested order

| # | Item | Cost | Buys |
|---|---|---|---|
| 1 | Invariant set + random driver over the corpus (§1, §2) | 1–2 d | Foundation for four later items |
| 2 | Rehydration differential (§2) | ½ d | Crash-safety of the core boundary |
| 3 | FEEL differential vs dsntk (§6) | 1 d | A stated must-not-change claim, verified |
| ~~4~~ | ~~Model generator + structural oracle (§3a, §3b)~~ | done | **Landed**: `tests/modelgen/` + `tests/generator.rs` |
| ~~5~~ | ~~Explicit-state exploration (§7)~~ | done | **Landed**: `crates/rbpmn-core/tests/explore.rs` |
| ~~6~~ | ~~Mutation fuzz (§3c) + restriction counterexamples (§3d)~~ | done | **Landed**: `tests/mutation.rs` |
| ~~7~~ | ~~Storm + replay verification + fsck (§4)~~ | done | **Landed**: `tests/storm.rs` |
| ~~8~~ | ~~Chaos (§5)~~ | done | **Landed**: `tests/chaos.rs` |
| 9 | Kani on `iso8601` + `condition` parsers (§7) | 2–3 d | Panic/overflow proofs on untrusted input |
| ~~10~~ | ~~TLA+ spec of the concurrency protocol (§7)~~ | done | **Landed**: `spec/`, `just tla` |

Items 3, 4, 5 and 6 are done, and they compose: the generator feeds both the
explorer and the mutation fuzz, all three sharing one invariant set. Of the
six third outcomes, 1, 2, 5 and 6 are now hunted; **3 and 4 are not, and they
are the ones that live in the Postgres layer** — which is also the newest and
most concurrent code in the repo, verified by hand-written cases. Items 7 and 8 are done, which closes the design brief's testing-strategy #5
and leaves **all six third outcomes hunted, none found**. What remains is
narrower and optional: item 1's random driver and rehydration differential,
item 9 (Kani on the parsers), extending the generator to boundary events and
event-based gateways (§3's status note), and the two protocol pieces the specs
do not yet model — the event-stream safe horizon and the deploy/undeclare
race. None of them is load-bearing for v1; all of them are cheap next to what
is already in place.

Item 1 is cheaper than listed — the invariant set already exists, leaving the
random driver and the rehydration differential. Extending the generator to
boundary events and event-based gateways (see §3's status note) is the other
open half of §3.
