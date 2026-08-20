# DMN & FEEL — the dsntk route

**Status: Gate 0a and 0b pass; 0c (the DMN TCK) is the remaining gate.**
Nothing is shipped. This document records the survey, the decisions taken
before any code, the measured results, and a tracker. The tracker at the
bottom is the authority on where we are; everything above it is why.

The headline: **the substitution works.** A pure-Rust decimal replaces Intel's
C library under the whole dsntk tree, 26 300 differential comparisons against
the C library find no unaccounted divergence, and the DMN stack parses,
compiles and *evaluates* a decision table inside a WebAssembly VM.

**Read D10 before anything below that mentions `[patch.crates-io]`.** The
substitution began as two shim crates plus a workspace-global patch; it is now
two *features* of a dsntk fork, and rbpmn patches nothing. What was measured
did not change — the same decimal, the same deviations, the same numbers — but
where it lives did, and most of this document predates the move.

The design brief (`bpmn-engine-design.md`, then "Post-v1: decisions", now
"Decisions", plus the phase-8 note) said this had to be decided *before* the
spike, not during. This is that decision, written down. The brief now carries
the outcome and, in particular, the one prediction it got wrong.

---

## Why this document exists separately

Three things made DMN a design problem rather than a dependency choice:

1. **`rbpmn-model` and `rbpmn-core` must compile to wasm32.** They power the
   playground, the bpmnlint plugin and the editor's L2 check. dsntk cannot
   reach wasm32 as published.
2. **One verdict, one implementation.** `just parity` compares native Rust
   against WASM over the whole corpus precisely so a surface cannot report a
   different verdict than deploy will. A native-only DMN validator would break
   that by construction.
3. **The linter is the product's front door.** "Parse/validate all DMN + FEEL
   at deploy time" is not negotiable, so the validator has to exist on both
   sides of the WASM boundary.

---

## What was surveyed (dsntk 0.3.0, published 2026-04-29; surveyed 2026-08-14)

All 14 published crates were downloaded and read. The numbers below are
measured, not recalled — re-measure before trusting them against a later
release.

| Crate | src lines | Note |
|---|---:|---|
| `dsntk-model-evaluator` | 38 248 | builds every evaluator up front |
| `dsntk-feel-evaluator` | 30 386 | **carries the one reqwest** |
| `dsntk-feel-parser` | 12 881 | |
| `dsntk-feel` | 6 040 | values, contexts, `Bif` enum |
| `dsntk-model` | 4 895 | DMN XML → `Definitions`, via **roxmltree** |
| `dsntk-recognizer` | 3 795 | |
| `dsntk-feel-temporal` | 2 628 | **`chrono::Local::now()`** |
| `dsntk-feel-number` | 821 | **the only C dependency** |
| others | ~1 000 | common, macros, grammar, regex |

**The wasm blockers are exactly two, and both are small.**

- **`dfp-number-sys`** (Intel decimal C library, `cc-rs`) reaches the tree
  through *one* crate, `dsntk-feel-number`. That crate is **821 lines**: a
  single newtype `FeelNumber(BID128, bool)` over **35 distinct `bid128_*`
  calls**, whose entire public API is that one type. It ships **1166 lines of
  its own tests**.
- **`reqwest`** appears in *one file*,
  `dsntk-feel-evaluator/src/evaluator_java.rs` (92 lines), with two call sites:
  a `LazyLock<reqwest::blocking::Client>` and one match arm at
  `builders.rs:2722`. It POSTs to `http://127.0.0.1:22023` to call **Java
  methods from inside a FEEL expression**. `reqwest::blocking` does not exist
  on wasm32, so this is a hard compile failure — and the feature is one we
  would refuse on determinism and security grounds anyway.

**A third blocker that is semantic, not a build error:** `chrono::Local::now()`
in `dsntk-feel-temporal` (`defs.rs`, `feel_date.rs`, `feel_date_time.rs`) plus
`bifs/core.rs::now()`, reachable as `Bif::Now` / `Bif::Today`. The engine's
whole determinism story ("time enters as command data, never from a clock")
forbids these, and the *node's local timezone* silently deciding a business
rule is worse than the non-determinism alone.

**Everything else is wasm-clean**: serde, serde_json, regex, chrono, chrono-tz,
uuid, uriparse, url, urlencoding, petgraph, roxmltree (the same parser rbpmn
already uses), convert_case, antex. `std::fs` appears only in `#[cfg(test)]`
code. The PMML external-function path is a stub that returns a formatted
string — shipping it silently would be worse than refusing it.

**Licence:** the GitHub repo shows Apache-2.0, but all 14 *published crates*
declare `MIT OR Apache-2.0`. rbpmn's dual licence survives a fork or a vendored
patch. Verify this again on any version bump.

**Upstream health:** repo pushed 2026-07-01, 26 stars, 25 open issues,
effectively single-maintainer — hence pinning, and hence the differential
tests below rather than trust.

**API shape we build on** (all verified in the source):

- `dsntk_model::parse(&str) -> Result<Definitions>` — roxmltree, validates
  against the DMN XSD first. Namespaces: DMN 1.3 / 1.4 / 1.5.
- `ModelEvaluator::new(&[Definitions]) -> Result<Arc<Self>>` — builds every
  evaluator eagerly, and `build_literal_expression_evaluator` propagates
  `dsntk_feel_parser::parse_expression(..)?`. **So "validate all FEEL at
  deploy" is exactly "build the evaluator", and the build artifact is what you
  then cache.**
- `Evaluator = Box<dyn Fn(&FeelScope) -> Value + Send + Sync>`, so
  `ModelEvaluator` is `Send + Sync` and cacheable per definition version.
- `DecisionEvaluator::evaluate(def_key, global, input_data, model_evaluator,
  &mut evaluated_ctx) -> Option<Name>`.
- `dmn-js` 17.10.1 writes DMN **1.3** (`dmn-moddle` 12.0.1) — inside dsntk's
  accepted range, so no version gap.

---

## Decisions

### D1 — the seam: an injected validator, not a dependency

`rbpmn-core` defines the trait and the diagnostics; `rbpmn-dmn` implements it;
both `rbpmn-engine` and `rbpmn-wasm` pass **the same implementation**.

```
rbpmn-model ──┐                 unchanged: roxmltree, serde, serde_json, thiserror
rbpmn-core  ──┤  defines DecisionValidator; check_deployable takes &dyn
              │
rbpmn-dmn ────┘  implements it. dsntk lives here and nowhere upstream of it.

rbpmn-engine  --feature dmn--> rbpmn-dmn    native evaluation, in-transaction
rbpmn-wasm    --feature dmn--> rbpmn-dmn    editor: same validator + evaluate
```

The CLAUDE.md prohibition stays true *as written* — `rbpmn-model` and
`rbpmn-core` keep their dependency sets — and gains a companion sentence:
**dsntk lives in `rbpmn-dmn` and nothing upstream of it may depend on it.**

### D2 — R2: swap the number type, keep dsntk otherwise unmodified

Not a fork of 100k lines and not a generated parser. A crate of ours exposing
`dsntk-feel-number`'s API over a pure-Rust decimal, substituted through
`[patch.crates-io]`, so the rest of dsntk compiles untouched. Backend:
**fastnum** (pure Rust, `no_std`, explicitly wasm-compatible, `exp`/`ln`/
`pow`/`sqrt`, only depends on `bnum`).

*(Superseded in mechanism by **D10**: the same implementation now lives in a
dsntk fork behind a feature, and there is no patch. "Not a fork of 100k lines"
turned out to be a false choice — the fork changes ~180 lines and vendors the
implementation this decision produced.)*

**The patch applies natively too.** Native and WASM must share number
semantics; an editor that computes different arithmetic than the engine is a
worse bug than having no editor.

Rejected alternatives are recorded below.

### D3 — the core never evaluates

`step` parks at the business-rule task and emits `DecisionRequested`; the
projection evaluates inside the *same transaction* and re-enters `step` with
`Command::CompleteDecision { result }`. Consequences, all of them wanted:

- `rbpmn-core` stays dsntk-free **by construction, not by care** — the same
  argument that made `TimerSource` and `TimerDue` distinct types.
- Replay needs no evaluator: `chaos.rs`'s re-derivation feeds the recorded
  result back as command data, exactly like a work-item completion.
- No extra transaction boundary, no work-item round trip, no poll for
  something that takes microseconds.

### D4 — a DMN artifact is part of the definition version

A deployed definition is BPMN + its DMN artifacts + its manifest. Instances
pin the definition version, so they pin the decisions that were in force.

Cost, accepted openly: a decision table shared by two processes is deployed
with each, and changing a rule is a redeploy. That *is* what "deploy is code"
already means here, but it is the opposite of DMN's usual sales pitch, so it is
the ruling most likely to be argued with later.

Consequence worth having: **`unresolved-decision` needs no environment.**
Unlike `unresolved-topic` there is no L3 for decisions — the editor's verdict
on the decision half is complete and offline, and the confidential model still
never leaves the browser.

### D5 — wiring in the manifest, artifacts in the bundle

`Bindings` gains `decisions: { elementId: { decision, result } }`. This is the
spot where every other engine writes `camunda:decisionRef`; XML purity holds.

The DMN XML travels *alongside* the manifest, not inside it — a decision table
escaped into a JSON string is unreviewable and `git diff` on a `.dmn` is not.
Atomicity is preserved at the deploy call, whose body **is** the bundle format:

```json
{ "bpmn": "...", "decisions": ["...", "..."], "bindings": { ... } }
```

No new format is invented: the editor's export is literally a deploy request.

### D6 — `serde_json/arbitrary_precision`, and measure the deviation

FEEL numbers are 34-digit decimals, `serde_json::Number` is `f64`, and
PostgreSQL `jsonb` numbers are arbitrary-precision `numeric` — so the loss sits
in the middle layer, and `1/3` trips it immediately.

**Decision (Timo): do it right — turn on `arbitrary_precision`.** Features
unify workspace-wide, so this is all-or-nothing and it touches
`condition::eval`, merge patch, and every golden trace's number formatting. It
gets its own spike before P3 commits.

Precedent: correlation keys are strings and exact integers because "floats have
no canonical spelling across a jsonb round-trip". Same trap, bigger blast
radius.

**Where numbers still deviate, document by how much.** Any measured divergence
lands in "Measured deviations" below with its magnitude. If it is too large, we
stop and discuss rather than absorb it quietly.

**Spiked and landed** — see "The `arbitrary_precision` spike" below. It cost
one semantic change that nothing in rbpmn touches, and it fixed a silent
truncation that was already live.

### D7 — reject the non-deterministic builtins at deploy

New rule `feel-deterministic`: `Bif::Now`, `Bif::Today`, `ExternalJavaFunction`
and `ExternalPmmlFunction` are refused, everywhere a FEEL expression can appear
in a DMN model. This is the front door, and it is also what makes the reqwest
removal semantically correct rather than merely convenient.

**Injection is the better long-run answer** — database time in, recorded in the
event, deterministic on replay, exactly the timer rule — but it needs a third
patch site deep in dsntk's bifs, so it is a follow-up, not a prerequisite.

### D8 — new rule ids (stable public API from the day they land)

`dmn-validates`, `feel-parses`, `feel-deterministic`, `decision-has-binding`,
`unresolved-decision`. Rule ids are never renamed; new ones are always allowed.

`no-unsupported-element` relaxes for `businessRuleTask` **only when the runtime
lands** — the exact ordering ruling already taken for expression timers.

### D9 — DMN is on by default (reversing the opt-in this work started under)

The brief that opened this work said the opposite: *"keep in a separate part of
the codebase to begin with and make this an optional feature, with explicit
opt-in (so not in default)"*. That was the right call **to begin with** — it is
how a 100k-line dependency earns its way in — and it is reversed now that the
work has landed and the numbers are in: 26 300 differential comparisons, 195/195
upstream tests, 3 391 TCK cases byte-identical on two corpora.

The reason to reverse it is the same one that set the shape of the whole
feature: a workflow definition plus its bindings manifest is meant to be a
**fully executable flow**, and a decision is part of that definition rather than
an add-on to it. Off by default made the ordinary build disagree with itself —
a server built with `ui` but without `dmn` shipped an editor that validated a
bundle's decisions and said it would deploy, against a server that refused it.

What is paid for it, stated rather than discovered:

- **The workspace MSRV becomes 1.94** (fastnum's floor), against 1.91 before,
  for **every** crate.

  A split was tried first and then removed, and the round trip is worth
  recording. The original note here claimed `--no-default-features` dropped the
  1.94 floor; a review pointed out it could not, because cargo checks
  `rust-version` *before* it resolves features. The fix was to make the claim
  true — `rbpmn-model`, `rbpmn-core` and `rbpmn-wasm` declaring 1.91
  themselves, plus a `just msrv` recipe and a CI task to hold it. Timo's call
  on reading that back: the split only ever helped a downstream consumer of
  those crates *standalone*, no such consumer exists, and everyone building
  this repo end-to-end needs 1.94 anyway because `just ui` compiles the editor
  with DMN. So: one floor, and the machinery is gone.

  Deleting the claim was always the other available answer, and it was the
  cheaper one. Worth remembering the next time a doc turns out to be wrong —
  making it true is not automatically the better fix.

  One check did *not* go with it: `just msrv` was incidentally the only thing
  building `rbpmn-wasm --no-default-features` for **wasm32**. That moved into
  `just no-dmn`, on the normal toolchain, where it belongs.
- **A plain `cargo build` compiles dsntk.** Accepted: `cargo test` now
  exercises the DMN paths by default (81 engine tests against 72), which is
  worth more than the build time.

What is **not** given up: the seam. `rbpmn-core` still takes a `&dyn
DecisionValidator` and still knows nothing of dsntk; D1 is untouched. The
feature still turns off, and `just no-dmn` is what stops that from becoming a
claim nobody checks — it asserts the dependency graph in *both* directions,
because both have been quietly wrong. Cargo unifies features per package across
the graph, so a single dependency edge taking the defaults switches `dmn` back
on for the whole build: a self dev-dependency in `rbpmn-engine` did exactly
that, and `cargo test --no-default-features` was running the DMN tests it was
meant to prove could be left out.

---

### D10 — a fork of dsntk, and no `[patch.crates-io]` at all

The two substitutions D2 and the reqwest wart describe are now **features of a
dsntk fork** (`github.com/tpjg/dsntk`), which `rbpmn-dmn` depends on by git
rev. `use-fastnum` is its default; `java-bridge` is off. rbpmn patches nothing,
and `crates/rbpmn-feel-number`, `crates/dsntk-feel-number` and
`crates/reqwest-shim` are gone — the implementation moved into the fork's
`feel-number/src/fastnum_number.rs` unchanged.

**What forced it.** Cargo honours `[patch]` **only from the workspace root of
the build being run**. A dependency cannot impose one and there is no manifest
key that asks for it — so an application depending on `rbpmn-engine` from its
own workspace and stopping there got a build that *succeeded*, silently, with
both substitutions gone: the C library back, wasm32 broken, and dsntk's FEEL
evaluator able to POST to a JVM again. That is precisely the failure this
project refuses everywhere else, and it was invisible.

The intermediate fix was a build script in `rbpmn-dmn` that proved the patch
was in effect and failed loudly with the lines to paste. It worked, and it was
the wrong shape: it made every consumer do the work and told them off for
forgetting. A feature travels down the dependency graph. A patch does not.

**Why a fork was cheaper than it looked.** The survey (and D2) rejected
"a fork of 100k lines" as the obvious non-starter. That was a false choice.
dsntk is a single Cargo workspace whose crates depend on each other by **path**,
so a git dependency on the fork resolves the *entire* tree from the fork —
measured: zero registry-sourced dsntk crates. And the change is ~180 lines
across four manifests and three source files, because the implementation
already existed. Upstream's own acceptance suite passes on both backends:
**191 assertions, 0 failures**, with one `pow` case written per backend because
the two disagree in the 34th significant digit.

**What did not change.** The decimal, its deviations, and every number in this
document. `just number-parity` reports the same 26 300 comparisons and the same
three deviation classes with the same counts, now comparing the fork's
`use-fastnum` against crates.io's C-backed `dsntk-feel-number` — the same
package name from two sources, which coexist because one is renamed.

**What it costs.**

- **A fork to maintain.** Mitigated by shape: both features are additive and
  default-flippable in one line each, so the fork is a viable upstream PR
  rather than a permanent divergence.
- **`just dmn-tck` loses its single variable.** A `[patch]` can no longer
  express the swap — the fork is `0.3.1-dev`, which does not satisfy the
  `^0.3.0` its own siblings request, so the patch silently did not apply and
  the recipe's own assertions caught it. `patched/` now builds the fork
  outright, so the comparison carries the decimal *and* 0.3.0 → 0.3.1-dev.
  Byte-identical verdicts prove both harmless together; a difference needs
  bisecting.
- **The rev is written in four places** (`crates/rbpmn-dmn`,
  `feel-number-parity`, `dmn-wasm-probe`, `dmn-tck/run.sh`). `just dsntk-rev`
  checks they agree and `just number-parity` depends on it, because a
  differential against a rev nobody ships is green and meaningless.
- **Feature unification cuts both ways.** Any crate in the graph asking for
  `java-bridge` restores an HTTP client for everyone, silently. `just no-dmn`
  now asserts the resolved tree carries neither `dfp-number-sys` nor
  `reqwest 0.13` — verified by switching the feature on and watching it fail.

## Rejected alternatives

- **dsntk native-only, no DMN in the editor.** Breaks one-verdict-one-
  implementation and leaves two FEEL grammars forever. The design brief
  pre-rejected it.
- **`sutra-feel` / `sutra-dmn`** (found during the survey; pure Rust, no C, no
  reqwest, `forbid(unsafe_code)`, MIT OR Apache-2.0, 11.4k lines, TCK L2
  126/126 and L3 3349/3369, with a determinism denylist, an injected clock and
  a path extractor that all read like they were written from this brief).
  **Held as the fallback, not chosen**: it was created 2026-08-06 with 0 stars
  and 13 downloads at `0.2.0-rc.1`, and it implements **DECIMAL64 — 16
  significant digits** where the spec pins decimal128/34. If dsntk does not
  work out, this is where we look first, with dsntk demoted to the differential
  oracle.
- **Write our own FEEL/DMN.** ~100k lines of measured evidence says no. The
  parser is not the hard part; the evaluator and the number semantics are.
- **Vendoring the whole dsntk tree.** 100k lines in-tree, linted and formatted
  by `just lint --workspace`, for a two-file problem.
- **Patching `reqwest` itself with a stub.** `[patch]` is workspace-global and
  `rbpmn-engine`'s `http` feature needs the real one. Fine inside an excluded
  probe workspace, never in the main one.

---

## Gates and phases

### Gate 0 — the number swap (go / no-go; everything depends on it)

Build `crates/rbpmn-feel-number` (the implementation, our own package name) and
`crates/dsntk-feel-number` (a three-line facade whose *package* name is
`dsntk-feel-number` 0.3.0, so `[patch.crates-io]` can target it). The split
exists so the differential harness can link **both** implementations at once —
two packages with the same name and version cannot coexist in one graph.

Three acceptance gates:

| | Gate | How |
|---|---|---|
| G0a | it computes the same numbers | `feel-number-parity/` (outside the workspace, like `feel-parity/`) differentials ours against the real `dfp-number-sys`; plus upstream's own 1166 lines of tests, vendored as our acceptance suite |
| G0b | it links for wasm32 | the DMN stack builds for `wasm32-unknown-unknown` with the number patch applied and the Java bridge gone |
| G0c | conformance survives | the DMN TCK (`DecisionToolkit/dsntk-test-runner` + the public `tck` mirror) run against the patched stack, compared to upstream's published 3374/3391 |

Divergence confined to the transcendentals (`exp`, `ln`, `pow`, `sqrt` — all
`Option`-returning upstream) is acceptable **as a documented, measured
deviation**. Anything else stops the gate.

### P1 — `crates/rbpmn-dmn` — **done**

Shipped: `Validator` (`dmn-validates`, `feel-parses`, `feel-deterministic`),
the expression walk, the value bridge, `Decisions` for evaluation, a 17-file
fixture corpus with the same `expect-diagnostics:` contract as the BPMN one,
and `DecisionValidator` in `rbpmn-core` — a trait, so the core's dependency
set is untouched and `rbpmn-model`/`rbpmn-core`/`rbpmn-wasm` still compile to
wasm32 (verified).

`NoDecisions` is the validator a build without DMN uses, and it **refuses**
bundled artifacts rather than ignoring them: a deployment carrying decisions
this binary cannot check is not a deployment that "has no decisions".

Two things P1 was asked to find, and both answers were the opposite of the
assumption. They are in "What P1 measured" below.

### P2 — deploy carries the DMN — **done**

`Bundle` is the triple — process, manifest, artifacts — and it *is* the HTTP
body, so the library path and the wire path cannot drift into validating
different things. `Engine::deploy(xml, bindings)` stays as a thin spelling of
`deploy_bundle`, not a second implementation.

Artifacts persist in `rbpmn_definition_decision` (`on delete cascade`, so a
decision cannot outlive its definition — the database enforces it rather than
this code remembering), in the same transaction as the definition row, and
they are part of the content hash: changing a rule allocates a new version
exactly as changing the diagram does, and old versions keep the decisions they
were validated with.

`unresolved-decision` resolves manifest bindings against the bundle, and
**refuses ambiguity rather than picking** — two artifacts defining the same
name make the binding ambiguous, which is `correlate`'s discipline applied to
decisions. `decision-has-binding` covers the *well-formed* half now (the
result must be a FEEL qualified name); its "a business-rule task must *have* a
binding" half cannot fire while `no-unsupported-element` still rejects every
model containing one, and lands with the runtime in P3 — the ordering ruling
already taken for expression timers.

Startup re-validation covers decisions: artifacts persist but the code that
validates them does not, so a binary rebuilt without the feature, or a dsntk
upgrade that stopped accepting an artifact, is exactly the drift this pass
exists to catch.

**The verdict cannot split.** `dmn` is a default feature of `rbpmn-server`
precisely because the editor it ships validates a bundle's decisions and tells
the user it would deploy; a server that then refused that bundle would be the
divergence the whole seam exists to prevent. Where the feature is *off*, a
bundle carrying artifacts is refused rather than accepted — tested in both
directions.

**`spec/` re-read and re-run, not assumed.** `LockOrder.tla` models deploy
(`advisory(key) → definition rows`), and P2 adds a statement inside that
transaction. The child rows stay the same `defn` resource, deliberately: they
are created by the transaction that creates their parent, reachable only
through it, and their foreign key takes KEY SHARE on a row this transaction
just inserted, so no other actor can hold it — and `delete_definition` reaches
them in the same order, so there is nothing to invert. The reasoning is
recorded in the spec next to the transaction shapes; `just tla` is green,
including all six expected counterexamples.

### P3 — core + engine — **done**

A business-rule task runs. `step` parks with `WaitKind::Decision` and emits
`DecisionRequested`; the projection evaluates *inside the same transaction*
and re-enters with `Command::CompleteDecision`; the answer **replaces** what
is at the bound path. The core never evaluates anything, so a replay reads the
recorded answer instead — which is what lets `chaos.rs` re-derive every
history through a core that has no evaluator.

*Replacement, not a merge patch* — a correction to what this section said
first, and it was a real bug. RFC 7386 cannot express what a decision means:
`null` in a merge patch **deletes** the member, so a null answer removed the
bound path instead of storing null there, and a gateway reading `result = null`
was then reading a missing value that compares the same way for a different
reason. Merging an *object* answer keeps keys from the previous run, so a
decision inside a loop reports a result it never produced. Consequently a
decision emits **no `variables-patched` event at all** — `decision-evaluated`
carries the answer, and anything reconstructing commands from history has to
know that (`docs/stress-testing.md`, replay verification).

`step_answering_decisions` **loops**, and more than one token can be waiting:
the core parks a branch on its decision and carries on with the rest of the
step, so a parallel split into two business-rule tasks parks both. An answer
can also advance a token straight into another business-rule task. Draining
until none is pending is what makes a chain of decisions one transaction
rather than a wedge. Decisions are cached per definition id alongside the
compiled process, and evicted with it.

*`WaitKind::Decision` must not outlive a step.* It is a request, not a resting
place: persistence refuses to write one (the `wait_kind` CHECK constraint has
no member for it either), and `Advancer::freeze` takes any still-pending
decision with it. That last part was missing, and the failure was worse than a
wedged instance — a freeze in one branch left a sibling parked on a decision,
the drain loop answered it on a `Failed` instance, and the resulting
`InstanceNotActive` rolled back the transaction that was *recording the
freeze*. No instance, no incident, and every retry identical.

**Two rulings taken here, both narrower than the plan assumed.**

*A failed decision freezes; it is not catchable by an error boundary.*
Boundaries match an error **code**, and a failed decision has none to give —
DMN has no error codes. Catching one would mean inventing a reserved code and
teaching modelers to write it in their BPMN, which is a designed contract, and
a feature is never the reason one ships early. So the token parks at the
element with the uniform incident shape and inspection shows *where*.

*A null answer continues.* Freezing on null would turn every incomplete
decision table into an incident, and P1 measured that dsntk cannot tell a legal
"no rule matched" from a broken evaluation anyway. The result is written as
JSON null and the model decides what that means — a gateway on
`result = null`. What *is* unambiguous, a value the variable document cannot
hold, freezes: dropping a decision's answer silently would be worse.

**The state-space explorer caught a real mistake.** It reported a token
awaiting a decision as a deadlock, and the first instinct — "the engine
resolves it inside the transaction, so it never happens" — was wrong at the
level the explorer works: the *core* does observe that state, between the two
steps. It now supplies `CompleteDecision` as a stimulus with both a value and
the `None` that freezes, so decision paths and the incident branch are walked
rather than excused.

**`spec/` re-read, not assumed.** P3 adds no lock, no claim and no lease:
evaluation runs inside the transaction that already holds the instance row,
reads `rbpmn_definition_decision` with a plain SELECT (ACCESS SHARE on the
table, not a row lock the model represents), and waits for nothing. The token
it parks is resolved before that transaction commits, so it is never a state
another actor can contend for. Recorded in `LockOrder.tla`; `just tla` green,
11 of 11.

### P4 — WASM + parity — **done**

`check_deployable` takes the bundled DMN artifacts and an injected
`DecisionValidator`; `rbpmn-wasm` gains a `dmn` feature (**on by default** —
an editor that cannot validate a bundle's decisions reports a different
verdict than deploy will) and a third export, `evaluate_decision`, for the
editor's try-it pane.

**Parity now covers decisions.** The DMN corpus runs through
`check_deployable` on both sides, native and WASM, compared byte for byte:
133 checks, up from 116. That corpus is where a divergence is *most* likely,
because it is the only path that runs dsntk — including a decimal
implementation this project substituted — so leaving it out would have left
out the only part with a plausible reason to differ between targets.

The doc called matching feature sets "the one place the plan can rot
silently". It does not, and the reason is `NoDecisions`: a build without the
feature **refuses** bundled artifacts rather than ignoring them, so a
mismatch changes the output rather than emptying it. Verified by building one
side each way — all 17 DMN fixtures differ, and parity fails per fixture. A
surface with zero fixtures is also treated as a failure, so the corpus cannot
quietly vanish.

One consequence worth stating: the editor document is now **16 MB** (5.8 MiB
of wasm before base64, plus bpmn-js). Asset size is explicitly not a concern
for these admin/debug surfaces, and the *inspector* is untouched at 219 KiB
because it deliberately carries no linter. If it ever does become a concern,
the lever is `--no-default-features` for a lint-only editor, which the parity
check will then hold to a matching native build.

### P5 — the editor — **done**

dmn-js embedded whole — DRD, decision table, literal expression, boxed
expression — on a second canvas, with a Process/Decisions switch. Side by side
was rejected: a decision is edited *because of* a task in the process, not
alongside it, and two live canvases double the ways focus goes missing.

The working set is a process, a manifest and N artifacts. The individual files
stay the primary form — they are what a git diff can show — and `Bundle` is
for handing the whole deployment over. That bundle is not a format the editor
invented: it is exactly the `POST /v1/definitions` body and exactly what
`rbpmn_engine::Bundle` deserializes, so what the editor exports is what deploy
consumes, with no converter in between to disagree with either end.

A business-rule task gets two wiring rows, decision and result path, and the
decision row offers **the names the bundle actually exposes** — no server
asked, because a deployment's artifacts travel inside it. The try-it pane runs
the same evaluator the engine runs; a null answer is shown as an answer with
its reason as explanation, never as a verdict.

**`just e2e-ui` earned its keep, twice.**

First: the editor was **completely dead** and everything else was green.
dmn-js's decision table is built on Inferno, which reads
`process.env.NODE_ENV` unguarded at module scope; Vite substitutes that for
app builds but not for library builds, so the document threw
`process is not defined` on load and rendered nothing. `cargo test`,
`just ui-test` and the document-structure tests all passed against a blank
page. One `define` in `vite.config.js` fixes it — and nothing but a browser
was ever going to find it.

Second: adding a textarea to a new pane silently shifted the positional
selectors (`textarea.code` nth(1)) the existing checks used, because the
try-it pane only renders once a decision exists. The textareas now carry
stable classes instead.

The suite covers the authoring loop end to end: a business-rule task with no
binding is refused, a new decision renders its DRD with drill-down, the wiring
pane then offers it by name, binding it clears both decision diagnostics, the
manifest updates, and the try-it pane evaluates it.

The editor document is now 16.9 MB of JS plus 603 KiB of CSS. The inspector is
untouched at 219 KiB, because it deliberately carries no linter.
### P6 — the paperwork this project treats as load-bearing — **done**

`bpmn-engine-design.md`'s "Post-v1: decisions" is now "Decisions", carrying
what was decided *and* the prediction it got wrong (that a native-only crate
would sidestep the wasm32 constraint — the editor is why it did not).
`CLAUDE.md` gains the `rbpmn-dmn` rule, the two `[patch]` substitutions and
why neither may be removed, and the DMN-default seam.
`.build.yml` gains `no_dmn` and `number_parity`; `dmn-tck` is the one
owed command deliberately left off, and says so. README gains the five rule
ids, the four new crates, the `Bindings` wiring table and the owed-commands
rows. This file gains the deviation table.

The pass also swept every document and long-form comment for claims the code
contradicted, which is what P6 is actually for.

### What the paperwork found

P6 is not a formatting pass. Two review rounds had just established that this
repo's expensive bugs live in prose the tests cannot read, so the sweep was
mechanical: extract the claims, check each against the code that would falsify
it. Nine findings, none of which any suite could have caught.

| Claim | Reality |
|---|---|
| `no-unsupported-element`'s message: business-rule tasks are "planned post-v1 via DMN; until then compute the decision in application code" | Dead arm behind a `NodeKind` supported since P3 — and the README repeated it in the rule catalogue |
| `map_topic` / `map_correlation`, named in 8 places across the README, the brief and three crates | Neither exists. The shipped API is a `Bindings` builder. One of the 8 was a user-facing lint message; another was the README's wiring table, an API reference to functions nobody can call |
| The README's rule catalogue | Missing `decision-has-binding` and `unresolved-decision`. Diffed against the code: 22 rules, all present now |
| `check_deployable(xml, bindings)`, deciding "four things without touching Postgres" | Three arguments and a validator; six things |
| "24 accept / 34 reject" fixtures; `just parity` over "52 fixtures" | 27/34, and 60 |
| The workspace table | Listed none of `rbpmn-dmn`, `rbpmn-feel-number`, `dsntk-feel-number`, `reqwest-shim` |
| **This file**: a decision's answer lands "as a merge patch" | Fixed in 2228cb8 — it replaces. The record was stale about its own correction |
| `docs/stress-testing.md`: "3 and 4 are not [hunted]", two sentences before "all six third outcomes hunted" | Self-contradictory in one paragraph. It also left the FEEL differential unstruck though it shipped, and proposed `dsntk-feel` as a dev-dependency — which CLAUDE.md forbids, because a dev-dependency still enters the workspace lockfile and takes the C library with it |
| The persistence guard: committing a `WaitKind::Decision` would "write a `wait_kind` no loader understands, wedging the instance permanently and silently" | `rbpmn_token`'s CHECK constraint has no such member, so the database refuses it. Three gates — a non-exhaustive match that will not compile, this error, and the constraint — not a silent wedge |

One thing recorded rather than left to be discovered: **the model generator
emits no business-rule tasks.** Decisions are exercised by the fixture corpus,
the explorer and the engine's integration tests, but not by search. That gap
has teeth — the freeze that stranded a sibling on `WaitKind::Decision` needed a
parallel split with a decision on one arm, and no generated model can produce
one. It was found by review.

---

## Known warts, stated up front

- ~~**`[patch.crates-io]` is workspace-global and feature-blind.**~~ **Resolved
  by D9.** The notice (`Patch ... was not used in the crate graph`) appeared
  when the `dmn` feature was off; with it on by default the patch is always in
  the graph. Measured after the flip: no notice in either the default build or
  a `--no-default-features` one.
- ~~**The patch pins us to dsntk's minor version.**~~ **Superseded by D10.**
  The pin is now a git `rev` on the fork, which is the same handle with a
  sharper edge: it is written down in four places and `just dsntk-rev` is what
  keeps them equal.
- ~~**The reqwest removal has a home: `crates/reqwest-shim`**~~ **Superseded
  by D10** — it is the fork's `java-bridge` feature, left off, so no HTTP
  client is compiled at all. The shim's own sharp edge is worth keeping on
  record, because it is why the feature is the better answer: a package
  *named* `reqwest` replaces reqwest **graph-wide**, so an application that
  itself asked for 0.13 got the shim. Measured: async use failed to compile
  (loud), and a blocking POST — the exact call dsntk makes — compiled and
  failed at run time with rbpmn's message about the application's own request.
  The historical description follows. A package *named* `reqwest`, pinned to
  dsntk's `^0.13`, patched in exactly like `dsntk-feel-number`. It has the shape the Java bridge calls and its
  `send` always fails, so the capability does not exist rather than being
  politely declined — which is the point: rbpmn does not want a decision
  calling out to a JVM, or to anything else.
  The version pin is the sharp edge and is commented in the manifest: reqwest
  0.12 and 0.13 are separate packages, so the patch replaces dsntk's copy and
  leaves `rbpmn-engine`'s `HttpPostHandler` on the genuine 0.12 from
  crates.io. Bumping the shim to satisfy a future `^0.12` dependency would
  silently defang that handler.
  Rejected: vendoring `dsntk-feel-evaluator` (30k lines of someone else's code
  in-tree, and upstream bumps become a merge instead of a rebuild). Still
  worth doing: an upstream feature gate for the Java bridge — the crate
  declares no features today, so it is purely additive, and it would shrink
  this patch to a flag.
- **`dsntk-model` depends on `dsntk-examples`** (290 KiB of sample DMN) for its
  in-`src` tests. Dead weight in the blob. Asset size is explicitly not a
  concern for these admin/debug surfaces, so this is a note, not a problem.
- **`dsntk-feel-grammar` has a build script touching `std::fs`/`std::process`**
  — build-time only, but confirm it behaves under cross-compilation in G0b.

---

## Gate 0 results

### G0a — the number swap (`just number-parity`)

**Upstream's own 1166-line test corpus**, vendored verbatim into
`crates/rbpmn-feel-number/tests/` with only the crate name changed: **195 of
195 pass**. The single case upstream asserts that this implementation does not
reproduce (`test_pow_002`, a fractional power's 34th digit) moved to
`tests/deviations.rs`, where both answers are pinned.

**The differential** (`feel-number-parity/`, linking our implementation *and*
the C-backed original): **26 300 comparisons, 0 unaccounted divergences.**

| Suite | Comparisons | Named deviations |
|---|---:|---|
| literals (parse + render + canonical form) | 87 | none |
| parsing (acceptance, and the stored scale) | 130 | 4 zero-rendering |
| constructors | 70 | none |
| comparisons (`=` `<` `<=` `>` `>=`) | 7 605 | none |
| integer conversions | 156 | none |
| formatting flags (`{:.2}`, `{:>12}`, `{:+}`) | 156 | none |
| arithmetic (`+ - * / %`, value and canonical form) | 15 210 | 237 reference-panics, 258 zero-rendering |
| scaled rounding (6 functions × 6 scales) | 1 638 | 64 reference-panics |
| unary (`abs`, `trunc`, `frac`, `exp`, `ln`, `sqrt`, predicates) | 585 | 15 transcendental, 9 zero-rendering |
| powers | 663 | 61 transcendental, 58 zero-rendering |

Exact arithmetic, comparison, rounding, parsing, rendering and conversion
agree with the C library **digit for digit**. Every deviation falls in one of
three classes:

| Class | Count | What | Magnitude | Verdict |
|---|---:|---|---|---|
| Transcendental tail | 76 | `exp`, `ln`, `sqrt`, `pow` differ in the last digits | **worst 1.30e-30** (`pow`, large integer exponent); `ln` 7.24e-34, `exp` 4.54e-34 | **Accepted.** Two independent implementations at 34 digits; DMN does not require bit-equality here. The tolerance is applied at those four call sites only, so it cannot absorb a drift in exact arithmetic. |
| Zero rendering | 329 | A zero result renders as `0` for us and `0`/`00`/`000` for the reference | **exactly equal values** | **Accepted, deliberately not reproduced.** The reference expands a positive exponent into zeros, so `0 / 0.1` renders `00` — and since `Jsonify` is `Display`, that is what it emits into JSON, which no parser accepts. |
| Reference panics | 301 | The C library **aborts**; we answer | n/a | **Accepted.** Two upstream bugs: `Display` unwraps a split on `E` that `+Inf`/`NaN` do not contain, and `validate_scale` adds `bid128_ilogb` (which is `i32::MIN` for zero) to the scale in checked `i32`. |

Not emulated, and stated rather than discovered later: decimal128's **gradual
underflow** into subnormals between 1e-6143 and 1e-6176. The flush-to-zero
boundary itself *is* emulated, because a decision comparing a result to zero
must not answer differently on the two implementations.

### What the differential found in *our* dependencies

None of this was visible from the outside, and all of it is why the check
exists rather than an argument that the swap is safe.

**fastnum 0.7.5** (five defects, all worked around in `number.rs` — the fifth
came out of the second pass below):

1. **Half-even rounding loses the sticky bit.** It decides from the first
   discarded digit alone, so `0.5000001` rounds to `0` and `2.5000001` to `2`.
   `ceil`, `floor`, `trunc` and `HalfUp` are unaffected. Since this is exactly
   the rounding decimal128 needs after every operation, that path is
   reimplemented on the digit string. **Worth reporting upstream** — it
   silently corrupts money arithmetic in any caller.
2. **`-0` sorts below `0` while comparing equal to it**, so `<` and `==`
   disagree about signed zeros. Every comparison settles the both-zero case
   first.
3. **`sqrt` overflows to infinity well inside range** — `sqrt(1e600)` answers
   infinity for a value that is a plain `1e300`. The exponent is halved before
   the call and restored after, which also makes exact powers of ten exact.
4. **`pow` and `exp` do not saturate outside the format's range**: `10 ** 39995`
   *panics* despite traps being disabled, and `exp(1e6000)` **overflows the
   stack and aborts the process**. Arguments whose result leaves decimal128's
   range are answered directly.

Plus three IEEE deviations fastnum shares with nobody: `x ** 0` is NaN rather
than 1 for `x = 0`; `0 / 0` is infinity rather than NaN; a negative base with
a fractional exponent answers as if the base were positive; and
round-to-integral of a non-finite value answers NaN rather than the value.

### The second pass: what a corpus gap was hiding

The numbers above are from the corpus *after* a code review pushed back on
five specific claims. Four of the five were right, and the fifth was a hole in
the harness rather than in the code — which is worse, because a hole reads as
a pass.

**The hole.** Every operation loop skipped a literal either side refused
(`let (Some(t), Some(o)) = … else { continue }`). An implementation that
*accepts* what the C library rejects was therefore skipped in silence.
`parsing_agrees` now checks acceptance itself, and it immediately found that
`FromStr` answered `Infinity` for `1e6145` — fastnum's type is far wider than
decimal128, so the literal parsed perfectly finite and only overflowed when
clamped into range, and the finiteness check ran *before* the clamp. Every
later operation inherited that infinity.

**The gap.** The corpus had no value that is exactly representable in
decimal128 *and* wider than 34 characters. That one shape is what separates a
34-significant-digit remainder from an exact one, and with it:

| Divergence | Reference | Was | Now |
|---|---|---|---|
| `even(2000…050)` (35 digits, exactly representable) | `true` | `false` | `true` |
| `odd(2000…050)` | `false` | `true` | `false` |
| `(-1) ** 2147483648` | `1` | `-1` | `1` |
| `(-1) ** 1e30` | `1` | `-1` | `1` |
| `parse("1e6145")` | refused | `Infinity` | refused |
| `parse(" 1")` | `1` | refused | `1` |
| `parse("1e6144")` stored form | `1000…000E+6111` | `1E+6144` | `1000…000E+6111` |

All of them are fixed rather than recorded, which is a better outcome than the
review proposed. Three were ours:

- **Parity was decided by a rounded remainder.** `even`/`odd` went through
  `remainder(2)`, whose quotient rounds to 34 significant digits — so for a
  wider integer the last digit, the only one parity depends on, was gone
  before the remainder was taken. `exact_parity` reads it off the coefficient
  instead, and falls back to the old path when the question does not arise
  (non-finite, non-integer), so nothing already verified moved.
- **Overflow was checked before the clamp**, the `FromStr` case above.
- **decimal128's exponent field caps at +6111**, so the C library holds
  `1e6144` as `1000000000000000000000000000000000E+6111` — the same number in
  a different cohort. `Display` cannot tell them apart, but the stored scale
  is what every later operation rounds against, so `norm` now pads too. Also
  ours: `bid128_from_string` skips leading whitespace and fastnum does not.

One is a **fifth fastnum defect**, joining the four listed above: its `pow`
**loses the exponent's parity above `i32`**, so a negative base raised to
`2147483648` came out negative. The sign is now settled from the exponent's
own digits and the magnitude raised separately. (And `nearest_away` needed the
same non-finite guard `floor_i`/`ceil_i`/`trunc_i` already had — fastnum's
`rescale` answers NaN for infinity, which is the fourth item's IEEE note
reaching one more call site.)

The lesson is the one the project already writes down: a differential is only
as good as the shapes in its corpus, and a skip is not a pass. Both are now
structural — the parse suite cannot skip, and `PARSE_ONLY` exists precisely
for literals whose acceptance *is* the question.

### G0b — wasm32 (`just dmn-wasm-probe`)

With the number patch applied and the Java bridge stubbed out, the **entire
DMN stack compiles to `wasm32-unknown-unknown`**, and — built through
wasm-pack exactly as `rbpmn-wasm` already is — it **runs**:

```
module size   : 5.27 MiB (wasm-opt'd release)
compile()                                    -> 1 invocable found
evaluate(50) evaluate(99)                    -> 0        (rule: < 100)
evaluate(100) evaluate(250) evaluate(1000)   -> 10 25 100 (rule: Amount * 0.1)
bad FEEL is reported                         -> <ParserError> syntax error: 1 +
not XML at all is reported                   -> ok
```

That is a DMN document parsed, its evaluators built, and a decision table's
unary tests and FEEL arithmetic evaluated, in the browser's target.

**Three findings that change P1:**

- `ModelEvaluator::new` really is the deploy-time FEEL gate — a syntactically
  broken expression fails the *build*, with the parser's message. P1's
  `dmn-validates` / `feel-parses` rules are `parse` + `new`, nothing more.
- **The public evaluation entry point is
  `ModelEvaluator::evaluate_invocable(namespace, model_name, invocable, &input)`.**
  `DecisionEvaluator::evaluate` takes a `DefKey`, which the crate does not
  export, so the sketch in "P3 — core + engine" must use the former.
- **`dsntk_model::parse` calls `uuid::new_v4`**, to synthesize ids for
  elements that lack them — so it needs a host RNG (`crypto.getRandomValues`
  in a browser, which wasm-bindgen wires up), and parsing the same document
  twice does not produce the same internal ids. Harmless for diagnostics;
  worth knowing before anything keys off them.

### G0c — the DMN TCK (`just dmn-tck`)

**Pass, in the strongest available form.** The question was never "what does
dsntk score" — it is "does the substitution change a single answer" — so the
harness builds dsntk **twice** from the same pinned source, once as published
and once with our decimal patched in, and runs the whole TCK against both.

| Corpus | stock (Intel C library) | patched (ours) | result files |
|---|---:|---:|---|
| `dmn-tck/tck` @ `20274cd2` (2026-08-03) | 3361 / 3391 | 3361 / 3391 | **byte-identical** |
| ...plus `dsntk-tck-patches` @ `8dfc54e8` | 3376 / 3391 | 3376 / 3391 | **byte-identical** |

3 495 test results across 3 391 test cases and 154 DMN models, and **not one
verdict differs**. Totals alone would be the weak version of this check — two
runs can agree on the count and disagree on which cases failed — so the
runner's result files are compared byte for byte.

`dfp-number-sys` is **absent from the patched build graph entirely**: Intel's
C library, and with it the wasm32 blocker and the C FFI in a tree whose core
is `#![forbid(unsafe_code)]`, leaves the whole dsntk tree. The harness asserts
that in both directions before it believes any comparison — a `[patch]` that
silently failed to apply would otherwise produce two identical builds and a
triumphantly green result.

On the patched corpus the 15 remaining failures are **14 external-Java tests**
(`0076-feel-external-java`, which need a Java RPC server on localhost that we
deliberately do not run, and which `feel-deterministic` refuses at deploy
anyway) plus one `0085-decision-services` case. Upstream publishes 3374/3391
passed with 16 not-supported against this same corpus; we measure 3376
passing, which is consistent and leaves nothing unexplained.

Nothing is vendored. The TCK corpus is separately OMG-licensed, and the dsntk
source and test runner are pinned fetches with the crate tarball's checksum
verified before extraction — the `just tla` discipline, for the same reason.

---

## Gate 0 verdict: **pass**

The substitution is sound. A pure-Rust decimal replaces Intel's C library
under the whole dsntk tree through one `[patch.crates-io]` line; it agrees
with the library it replaces across 26 300 differential comparisons and
upstream's own 1166-line corpus; it changes not one of 3 391 DMN TCK verdicts;
and the resulting stack parses, compiles and evaluates DMN inside a
WebAssembly VM.

That retires the constraint this brief recorded as permanent — *"dsntk can
therefore never enter `rbpmn-model`"* — and with it the collision the phase-8
note flagged.

---

## The `arbitrary_precision` spike

**Verdict: landed.** One line in `[workspace.dependencies]`, and it is guarded
by `numbers_wider_than_f64_survive_the_variable_document` in
`crates/rbpmn-engine/tests/engine.rs` — which fails with a precise message if
the feature is ever dropped. That test is the whole reproduction; the
throwaway probe that produced the tables below was not kept.

### What it buys — and this was already broken

The premise was that DMN needs it. Measuring it showed the truncation is not a
future problem at all: it is live today, on the path between the application
and PostgreSQL, whose `jsonb` numbers are arbitrary-precision `numeric` and
were never the lossy part.

| Written by the application | Stored before | Stored now |
|---|---|---|
| `0.3333333333333333333333333333333333` | `0.33333333333333337` | exact |
| `123456789012345678901234567890` (an order id) | `1.2345678901234568e+29` — **an integer became a float** | exact |
| `1.50` (a price) | `1.5` | `1.50` |
| `1e10` | `10000000000.0` | `1e+10`, and `10000000000` after a `jsonb` round trip |

A 30-digit identifier silently turning into scientific notation is exactly the
class of "seems to run" this project exists to refuse, and it needed no DMN to
happen.

### What it costs

**One semantic change: `Value` equality becomes representation-based.**
`parse("1.5") == parse("1.50")` was true and is now false. Surveyed rather
than assumed:

- **Conditions are unaffected**, which is the only place a number decides
  where a token goes. `condition::compare` reads through `as_f64()`, never
  `Value` equality, so `trailing = 1.5` still holds for a stored `1.50`.
  Verified against the corpus and directly.
- **Correlation keys are unaffected**: `subscribe` renders the key with
  `as_i64`/`as_u64` and compares *strings*. Identical answers before and
  after, including the loud incident for a 30-digit key.
- **No `#[serde(flatten)]`** anywhere in a serialized type — the known
  `arbitrary_precision` footgun does not apply.
- `Value` equality appears only in test assertions, all of which still pass.

Nothing moved: `cargo test` (182), `just lint`, `just feel-parity`,
`just parity` (116/116 native-vs-WASM plus 57/57 through bpmnlint) and
`just ui-test` (31) are all green with the feature on.

### The finding that matters for P1

**Storage became exact; comparison did not.** The document now holds
`1.0000000000000000001` and `1.0000000000000000002` as distinct values, and
the FEEL-subset comparator answers `a = 1.0000000000000000002` **true** when
`a` is `…001`, because both sides go through `f64`.

That was not a regression — it is what the comparator always did — but it
stopped being invisible the moment DMN decisions began producing 34-digit
numbers and a gateway read one. **Closed in P3**: `Literal::Num` keeps the
number as written and `condition::compare` compares decimals, so
`a = 1.0000000000000000002` is now false for a stored `…001`. `just
feel-parity` stayed green, which is the point — the change moves the subset
*toward* FEEL, whose numbers are decimal128, not away from it.

A smaller one, worth revisiting when it costs something: the correlation-key
rule refuses anything that is not a string or an exact `i64`/`u64`, and its
stated reason was that "floats have no canonical spelling across a jsonb
round-trip". Under `arbitrary_precision` they do. The rule is unchanged and
still safe — an out-of-range key freezes loudly — but its rationale is now
weaker than its wording.

---

## What P1 measured

### The null distinction does not exist

This document told P1 to pin, before anything else, that dsntk reports a
runtime failure as `Null(Some(reason))` and a legitimate "no rule matched" as
`Null(None)`. **That is not what it does.** From `tests/outcomes.rs`:

| input to an incomplete UNIQUE table | answer |
|---|---|
| a score no rule covers — a legal gap | `Null("no rules matched, no output value defined")` |
| a string where the table compares numbers | `Null("no rules matched, no output value defined")` |
| the input missing entirely | `Null("no rules matched, no output value defined")` |
| an explicit null input | `Null("no rules matched, no output value defined")` |
| naming a decision that does not exist | `Null("invocable 'X' not found …")` |

Every null carries a reason, there is no bare null to be found, and the
*identical* reason covers a legal gap and a type error. Nothing in the value
can separate them.

So `Outcome` does not pretend it can. It has one `Null { reason }` variant,
and the reason is documented as diagnostic text rather than a signal —
branching on it would freeze an instance on an ordinary incomplete decision
table.

**The ruling for P3:** a null is an *answer*. The result is written as JSON
null, the token continues, and the reason goes into the event trace as detail
— the `timer-resolve-failed` precedent, where the prose reason lives outside
the stable `Display` format. Wiring that is genuinely broken is caught at
deploy (`unresolved-decision`, `dmn-validates`), which is where this project
puts that class of error anyway. A modeller who wants "nothing applied" to be
an error models it: a gateway on `decision = null`. That is "decisions
computed outside control flow, control flow reads results", applied.

### FEEL cannot be parsed without the model's scope

The determinism check walked the parsed AST first — an exhaustive 73-variant
match, no wildcard, so an upstream bump would break the build. It was
unsound anyway, and the fixtures caught it:

```text
parse("now()",          empty scope) -> FunctionInvocation(Name("now"))   found
parse("Amount + now()", empty scope) -> Name("Amount+now")                MISSED
```

FEEL names may contain spaces, `+`, `-` and `*`, so where a name ends depends
on which names are in scope. dsntk builds that scope from the model when it
compiles; anything parsing an expression on its own is guessing, and guessing
wrong lets a clock call through. The same scope-lessness made
`if Flag then now() else 1` fail to parse outright, which would have rejected
a valid model.

Two consequences:

- **`feel-parses` is dsntk's verdict, not ours.** `ModelEvaluator::new` parses
  with the right scope. Our walk runs only *afterwards*, to attribute that
  error to an element, and can never cause a rejection — only locate one.
- **`feel-deterministic` is lexical and conservative.** A name followed by
  `(`, outside string literals, with word boundaries. It errs toward refusing:
  a false positive is a rename, a false negative is a decision whose answer
  depends on which machine ran it.

Still the loud half only. The wall is removing the builtin from the evaluator,
which rides with the reqwest decision below.

### wasm32 was blocked on reqwest — resolved

*(Resolved right after P1 landed; kept because the measurement is what made
the decision obvious.)*

`rbpmn-dmn` does not yet compile to wasm32. One prerequisite is fixed —
`dsntk_model::parse` synthesises element ids with `uuid::new_v4`, so a browser
build needs `crypto.getRandomValues`, wired through uuid's `js` feature — but
`dsntk-feel-evaluator` still fails:

```
error[E0433]: could not find `blocking` in `reqwest`
  --> dsntk-feel-evaluator-0.3.0/src/evaluator_java.rs:9
```

That was not cosmetic: without it the editor cannot validate decisions
offline, and a reduced wasm validator would report a *different verdict* than
deploy, which breaks one-verdict-one-implementation. `crates/reqwest-shim`
settles it — see "Known warts". `rbpmn-dmn` builds for wasm32, and the Gate 0b
probe, now pointed at the shipped shim rather than a copy of it, still parses,
compiles and evaluates a decision table inside the VM.

---

## Tracker

Legend: `[ ]` not started · `[~]` in progress · `[x]` done · `[!]` blocked

### Gate 0 — number swap

- [x] `crates/rbpmn-feel-number` — fastnum-backed `FeelNumber`, full API
- [x] `crates/dsntk-feel-number` — facade package for `[patch.crates-io]`
- [x] upstream's 1166 lines of tests vendored as the acceptance suite (195/195)
- [x] `crates/rbpmn-feel-number/tests/deviations.rs` — every deviation pinned
- [x] `feel-number-parity/` — differential vs `dfp-number-sys` (outside workspace)
- [x] `just number-parity` recipe
- [x] **G0a** — 26 300 comparisons, 0 unaccounted; deviations measured and recorded
      (23 537 at the gate; the corpus grew twice under review)
- [x] `dmn-wasm-probe/` + `just dmn-wasm-probe` (throwaway; superseded at P1)
- [x] **G0b** — DMN stack parses, compiles *and evaluates* in a wasm VM
- [x] `dmn-tck/` + `just dmn-tck` — stock and patched builds, pinned and
      checksum-verified, compared case by case
- [x] **G0c** — 3 391 TCK cases on two corpora, byte-identical verdicts
- [x] **Gate 0 verdict: pass** — recorded above

### Owed once Gate 0 closes

- [ ] Report the four fastnum defects upstream (the half-even sticky bit is
      the one that matters to everyone else, not just us)
- [x] Reqwest removal: `crates/reqwest-shim`, patched in like the decimal
- [ ] Send the upstream feature-gate PR anyway — it would shrink our patch
- [x] ~~Decide the unused-`[patch]` notice~~ — moot: D9 put the patch in every
      default build, and the notice is gone from both builds (measured)
- [x] `.build.yml`: `no_dmn` and `number_parity` landed. `dmn-tck`
      stays off deliberately — it fetches the TCK, dsntk's source and a
      third-party runner — and both the README and CLAUDE.md now say so rather
      than claiming one task per command
- [x] README "owed by what you touched": `rbpmn-feel-number` → `just number-parity`,
      and a row for the `dmn` feature → `just no-dmn`

### Spike — `arbitrary_precision` — **done, landed**

- [x] blast-radius survey: conditions, correlation keys, merge patch, `flatten`
- [x] flip the feature, run the whole corpus, record what moved (nothing did)
- [x] `just feel-parity` still green
- [x] guarded by an engine test that fails without the feature
- [x] verdict recorded above
- [x] **done in P3**: `condition::compare` compares decimals, not `f64`

### P1 — `crates/rbpmn-dmn` — **done**

- [x] DMN fixture corpus + runner (17 files, embedded expectations)
- [x] parse + validate → `dmn-validates`, `feel-parses`
- [x] `feel-deterministic` over every FEEL-bearing field, plus external
      Java/PMML functions from `FunctionKind`
- [x] value bridge JSON ⇄ FeelValue + hostile corpus
- [x] the null question answered — and answered *no*; see "What P1 measured"
- [x] `DecisionValidator` + `NoDecisions` in `rbpmn-core`, implemented here
- [x] rule ids and catalogue entries in `rbpmn-model`
- [x] `just dmn-test`

### P2 — deploy — **done**

- [x] `Bindings.decisions` (+ `DecisionBinding { decision, result }`)
- [x] `Bundle` in `Engine::deploy_bundle` and the HTTP body — one struct
- [x] `rbpmn_definition_decision` migration (0010), `on delete cascade`
- [x] content hash covers all three artifacts
- [x] `unresolved-decision`; `decision-has-binding`'s well-formed half
      (its "present" half needs P3's lint relaxation)
- [x] duplicate decision names refused, never picked
- [x] `check_active_definitions` re-validates decisions
- [x] `delete_definition` — cascade proven by test
- [x] `dmn` feature on `rbpmn-engine`, `rbpmn-server` and `rbpmn-wasm` — **on by
      default**, with `just no-dmn` holding the seam open
- [x] `spec/` re-read; `just tla` green

### P3 — core + engine — **done**

- [x] `NodeKind::BusinessRuleTask`, parsed, linted and compiled
- [x] `Command::CompleteDecision`, `DecisionRequested`/`DecisionEvaluated`
- [x] golden scenario trace (`25-business-rule.json`) — the answer is *given*,
      so the trace needs no evaluator, exactly as a replay does not
- [x] `Decisions` cache keyed by definition id, evicted with the process
- [x] incident freeze on an unrepresentable answer; **not** boundary-catchable
      (no error code exists to match — see above)
- [x] state-space explorer answers decisions instead of calling them deadlock
- [x] `spec/` re-read and recorded; `just tla` green
- [x] `condition::compare` compares decimals, not `f64` — the comparator has
      caught up with the document (`just feel-parity` still green)

### P4 — WASM + parity — **done**

- [x] the reqwest home — `crates/reqwest-shim`; `rbpmn-dmn` now reaches wasm32
- [x] `check_deployable` gains decisions + an injected `DecisionValidator`
- [x] `evaluate_decision` export, behind the `dmn` feature
- [x] `just parity` covers the DMN corpus (133 checks) and fails on a feature
      mismatch — verified by building one side each way
- [ ] `dmn-wasm-probe/` can go once P5's editor drives the real export

### P5 — editor — **done**

- [x] dmn-js embedded, four view types, own canvas + mode switch
- [x] multi-file working set + bundle export/import (the deploy body itself)
- [x] decision bindings in the wiring pane, offering the bundle's own names
- [x] decision try-it pane on `evaluate_decision`
- [x] `just ui-test` (39), `just e2e-ui` — both halves, served included
- [x] the editor's `process is not defined` death, which only a browser found
### Review rounds

Two full reviews of the DMN work, both of which found things the tests did not.

- [x] **Round one** — 15 findings. Three contradicted claims made in this
      document: the scheduler could wedge an instance past repair, a null answer
      deleted the path it was meant to fill, and the feature defaults did not
      survive `cargo tree`.
- [x] **Round two** — 15 more, after the fixes. The sharpest was a *second*
      wedge in the same place: a freeze in one branch left a sibling parked on
      a decision, and the drain loop's `InstanceNotActive` then rolled back the
      transaction that was recording the freeze. Also caught the same ownership
      bug fixed in `captureDecision` and left in `decisionXmls`, an MSRV escape
      hatch that could not work, and — in the recipe written to stop exactly
      this — a `|| true` that reported the DMN seam intact for a `cargo tree`
      that had failed outright.
- [x] Both rounds' findings fixed, each with a check that fails without the fix.

The pattern worth keeping: in both rounds the most expensive findings were
**claims in comments and docs that the code contradicted**, not logic errors.
`just tla`, `cargo test` and the differential were all green through every one
of them.

### P6 — paperwork

- [x] `bpmn-engine-design.md` — "Post-v1: decisions" → "Decisions", carrying
      the outcome *and* the prediction it got wrong
- [x] `CLAUDE.md` — the rbpmn-dmn rule, both `[patch]` substitutions, the
      DMN-default seam, the full command list
- [x] `.build.yml` tasks
- [x] README rule catalogue (22 rules, diffed against the code) +
      owed-commands table + the four new crates + the `Bindings` wiring table
- [x] deviation table final
- [x] every document and long-form comment swept against the code — nine
      contradictions found and fixed, see "What the paperwork found"

### P8 — the fork (D10)

Not planned; forced by the discovery that `[patch.crates-io]` cannot reach a
downstream consumer. See **D10** for the reasoning and the costs.

- [x] `tpjg/dsntk` fork: `use-fastnum`/`use-dfp` on `dsntk-feel-number`
      (default fastnum), `java-bridge` on `dsntk-feel-evaluator` (default off),
      `uuid`'s `js` feature target-gated to wasm32 in `dsntk-common`
- [x] the implementation moved into the fork verbatim; upstream's suite passes
      on **both** backends (191 assertions, 0 failures), with `test_pow_002`
      written per backend
- [x] `rbpmn-dmn` depends on the fork by rev; `crates/rbpmn-feel-number`,
      `crates/dsntk-feel-number`, `crates/reqwest-shim` and the guard build
      script deleted; no `[patch.crates-io]` in the workspace
- [x] `feel-number-parity` differentials fork-vs-crates.io (same 26 300
      comparisons, same three deviation classes, same counts)
- [x] `feel-parity` and `dmn-wasm-probe` repointed; Gate 0b still passes, now
      with no patch at all
- [x] `dmn-tck` builds `patched/` from the fork rather than tarball+patch, and
      asserts both `dfp-number-sys` and `reqwest` are absent from it
- [x] `just dsntk-rev` (four pins agree; a dependency of `number-parity`) and
      `just no-dmn`'s new graph assertions, both verified by breaking them
- [ ] `just dmn-tck` re-run against the fork — **owed**, and the one gate this
      change has not yet passed
