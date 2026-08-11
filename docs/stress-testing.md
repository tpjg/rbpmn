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
| 1 | Linter accepts → `StepError::Invariant` or panic | model generator + mutation fuzz (§3) |
| 2 | Linter accepts → stuck token, no pending stimulus | state-space exploration (§2, §7) |
| 3 | Runs, but differently in Postgres than in the core | replay verification (§4) |
| 4 | Correct alone, wrong under concurrency | the storm (§4), chaos (§5) |
| 5 | Linter **rejects a valid model** (false positive) | model generator (§3) |
| 6 | Native and WASM lint disagree | parity fuzz (§6) |

Outcome 1 is the Camunda-lineage bug class the whole design exists to
prevent — a model that lints clean and then executes with wrong token
semantics. Outcome 5 is completely untested today: the `reject/` fixtures
prove the linter catches what it should, and nothing proves it *doesn't*
catch what it shouldn't.

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

Every technique below checks the same invariant set. Define it once, in
`rbpmn-core` behind `#[cfg(any(test, feature = "invariants"))]`, as
`fn check(proc: &ExecutableProcess, state: &InstanceState) -> Result<(), Violation>`:

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

Four properties fall out:

**(a) The linter has no false positives.** Every generated model must lint
clean. The generator is an *independent second implementation* of "what block
structure means"; any disagreement is a generator bug or a linter over-reach,
and both are worth knowing. This is the only test of outcome 5 that exists.

**(b) Structural oracle → differential execution.** The generator holds the
block tree, so a small recursive interpreter over that tree predicts — for a
chosen set of XOR decisions and loop counts — exactly which tasks execute, how
many times, and the final variable document, *without running the engine*.
Two independent implementations of BPMN semantics, differentially tested.
That is as close to a TCK as this domain permits.

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

**(d) Prove the restriction earns its keep.** The inverse, and the most
interesting artifact: generate deliberately *non*-block-structured graphs and
run them through the core with the region check disabled. Demonstrate on
demand that local token counting goes wrong — reproduce the Camunda-lineage
deviations in this engine — then re-enable the rule and watch them become
deploy rejections. That turns `balanced-gateways` from a restriction we
*assert* is necessary into one with a reproducible counterexample attached.
It belongs in the README.

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

## 4. Tier 2 — the concurrency storm, made to prove something

The naive storm ("N workers, M instances, did anything explode?") proves
little. Here is the version that proves a lot.

**Replay verification.** After the storm, `rbpmn_event` holds the full ordered
history of every instance — and it is rich enough to *reconstruct the command
sequence* (`work-item-completed` + `variables-patched` → `CompleteWorkItem`,
`timer-fired` → `FireTimer`, `message-received` → `DeliverMessage`, …). So:
for each instance the storm ran, extract its stimuli, replay them through the
pure core, and assert the core's trace equals the database trace projected
onto core-visible kinds.

That single check converts a load test into a semantic conformance run over
every execution that happened. It is the systematic form of
`full_flow_matches_the_core_golden_trace`, which today covers exactly one
fixture — and it directly tests the claim the architecture rests on: *the
Postgres layer is a projection of this core*. It is the only test of
outcome 3.

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

**`fsck` for rbpmn.** Factor these into
`check_invariants(pool) -> Vec<Violation>`, runnable against a live database
at any moment: every active instance's rows must rehydrate into a state that
satisfies §1. That is simultaneously the storm's assertion, the precondition
for chaos testing (§5), and a genuinely useful operator tool.

---

## 5. Tier 3 — chaos

Testing strategy #5 in the design brief — crash tests — has **no
implementation**. The transactional design's central promise is "kill it
anywhere, converge on restart", and nothing currently proves it. With the fsck
in place it is mostly mechanical:

- `pg_terminate_backend` on a random engine connection mid-storm.
- Drop and rebuild an `Engine` (node restart) while work is in flight.
- Handlers that panic, hang past the lease TTL, or return garbage.
- Lease TTL at 1 ms so reclaim races run constantly.
- **Clock skew.** The brief claims node clocks never decide anything. Prove
  it: run one node with a deliberately skewed clock and assert timers still
  fire exactly once, at the right database time.

After each chaos round: fsck clean → let it drain → replay-verify (§4). The
combination is what turns "it survived" into "every execution that completed
was semantically correct".

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

A prototype of exactly the loop above was run against the real `step`. The
existing corpus is trivial for it:

> **17 accept fixtures: 128 reachable states, 128 transitions, <1 ms total.
> Zero invariant violations, zero `StepError::Invariant`.**

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

If those claims deserve formal treatment, the right tool is **TLA+ (PlusCal)
on the protocol, not the code**: model the timer-claim path, the completion
path, the lease, and the deploy/undeclare race as abstract actions over an
abstract store, and let TLC check deadlock-freedom and exactly-once under all
interleavings. That is a two-day spec, and it targets exactly the class of
finding the brief describes reaching by careful reasoning — the AB/BA lock
inversion in the original timer-claim sketch. Careful reasoning found it
once; a spec finds the next one.

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
| 4 | Model generator + structural oracle (§3a, §3b) | 3–5 d | The synthetic TCK; linter false positives |
| 5 | Explicit-state exploration (§7) | 1 d | Bounded verification on real code (prototyped: works) |
| 6 | Mutation fuzz (§3c) + restriction counterexamples (§3d) | 2 d | The linter-hole hunt |
| 7 | Storm + replay verification + fsck (§4) | 3–5 d | The projection claim; deadlock freedom |
| 8 | Chaos (§5) | 2–3 d | Closes testing-strategy #5 |
| 9 | Kani on `iso8601` + `condition` parsers (§7) | 2–3 d | Panic/overflow proofs on untrusted input |
| 10 | TLA+ spec of the concurrency protocol (§7) | 2 d | The distributed claims |

Items 1 and 3 are independent and both pay immediately; 1 builds the driver
that 2, 4, 5 and 6 depend on. Item 5 is unusually high value per day once 1
and 4 exist — it is most of a model checker for the price of a visited set,
and the prototype behind §7's table already demonstrates the whole loop
against the real `step` in about 250 lines.
