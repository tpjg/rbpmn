# Per-element retry policy — design round

**Status: shipped.** Both slices.
The slices at the bottom are the staging; the decisions above them are why.

This round covers **a service task's retry budget and backoff becoming
deployment data** — how many times a handler is called before the instance
freezes on an incident, and how fast the gaps between those calls widen. It
was read against `crates/rbpmn-core/src/compile.rs` (`Bindings`,
`IndexDeclaration`, topic resolution), `check.rs` (`config_bindings`, the L2
verdict), `crates/rbpmn-engine/src/runtime.rs` (`persist_step`'s work-item
insert, `fail_work_item_in_tx`, the definition cache), `worker.rs`
(`next_retry_in`), `lib.rs` (`CLAIMABLE`, `retry_backoff`), `inspect.rs`,
migrations 0001/0002/0015, `ui/src/editor/manifest.js`, `spec/Lease.tla` and
`docs/design/task-config.md` (the precedent this follows).

---

## The motivating case, in one paragraph

An application has service tasks with genuinely different retry economics. A
service task with a fast local dependency: three tries at 5s, 15s, 45s
and it is either up or it is not. An service task that needs a person to
reset things is the opposite: nothing will change for hours, and the useful
policy is a handful of attempts spread across a couple of days. Today both
get the column default of 3 attempts and one engine-wide backoff base — the
budget is not reachable from a deployment at all (`retries` is never bound at
insert), and the base is a builder call on `Engine`, one value for every
definition in the installation. Retry economics are process policy. They
belong in the manifest beside the model, where every other runtime binding
already lives, and not in the embedding application's Rust.

## Four things reading the code turned up

**1. `retries` is already the total attempt count, and nothing says so.**
The fail path decrements first and tests after (`runtime.rs:419`), so a column
value of 3 means exactly three handler calls, not three retries after a first
try. That is the ambiguity D3 names the manifest member for.

**2. A topic defaults to the element id.** `Bindings::topics` is an override
table; an unmapped service task runs on a topic named after its element. This
is what decides D2: a single map that accepted "an element id or a topic
name" would resolve the wrong one for the most common wiring there is, and
silently.

**3. The backoff base is consumed inside SQL, not by a claimant.** It is an
argument to the `UPDATE` that computes `retry_at` from `clock_timestamp()`.
That is the difference from `config` — which task-config D6 deliberately kept
*out* of the work-item row — and it is what makes a column the right answer
here (D6 below).

**4. `next_retry_in` already bounds itself by `poll_interval`.** The worker's
early wake-up takes `min(until_retry, poll_interval)`, so a base measured in
minutes parks for one poll interval and re-checks. Confirmed rather than
assumed; a per-element base of `PT10M` costs the loop nothing.

---

## Decisions

### D1 — the policy is model content; the engine base stays the environment

A retry policy is part of what the model *does* — "this step gets seven tries
across two days" is a statement about the process, reviewable next to the
`.bpmn`. So it is manifest data: content-hashed, versioned, pinned with the
instance, and changing it is a deploy by construction.

`EngineBuilder::retry_backoff` stays exactly what it is: the installation's
fallback, for definitions that say nothing. That is the same split
`declare_topic` draws — the manifest says which topic, the environment says
what is registered — and it is why the new columns must mean "ask the engine"
when NULL (D6).

### D2 — one group, two layers: `by_element` over `by_topic`

```json
{ "topics":  { "send_notice": "send_message" },
  "retries": {
    "by_topic":   { "send_message": { "attempts": 7, "backoff": "PT10M" } },
    "by_element": { "lookup": { "attempts": 3, "backoff": "PT5S" } } } }
```

Resolution for a service task is element, then its resolved topic, then the
engine — and per *member*, not per policy: a `by_element` entry setting only
`attempts` takes its backoff from the topic layer if there is one, and from
the engine if there is not. Layers compose; they do not shadow wholesale.

**Why a topic layer at all.** The motivating flow has fourteen service tasks
on one topic (`send_message`), all wanting one policy. An element-keyed map
alone means the same object fourteen times in one manifest, which is the kind
of repetition that drifts — and it is repetition with no meaning, because
retry economics are a property of *the dependency being called*, and the
topic is what names that dependency. One entry says the true thing once.

**Why an element layer as well.** Topic alone cannot express two call sites on
one topic with different budgets — an urgent notice that should give up and
take an error boundary while the bulk reminder keeps trying. The workaround
would be splitting the topic, which grows the environment with *content*
rather than capability: the exact failure `docs/design/task-config.md` opens
by naming. Both layers are visible in the request; neither is imagined.

**Why they are nested rather than two flat groups.** They cannot share a key
space. An unmapped service task's topic *is* its element id, so a single map
keyed by "element or topic" would resolve the wrong layer for the commonest
wiring in the codebase, and would do it silently. Given they must be two maps,
one group for one concept beats two top-level groups whose relationship is
legible only from the docs. `by_element` / `by_topic` rather than
`elements` / `topics` because `topics` already means something else one line
above, and a manifest should not make a reader disambiguate by indentation.

**Rejected: a definition-wide default.** It removes the same repetition, and
it says something untrue while doing it. Retry economics belong to the
dependency, not to the process; a process-wide default couples every task that
happens to live in one diagram, and in the motivating flow it would have to be
overridden per element anyway the moment a second dependency appears. The
topic layer expresses what is actually shared.

**Why `config` has no topic layer and this does.** task-config D8 refused one
deliberately, and that still holds: config is per-call-site *by nature* — the
whole point is one handler configured differently at each call site. Retry
policy is per-dependency by nature — the dependency is as patient as it is,
wherever it is called from. Same shape, opposite grain, opposite default
layer.

### D3 — `attempts`, `backoff`, `multiplier`, every member optional

- **`attempts`** — total handler calls before the budget is spent, *not*
  calls-after-the-first. It maps 1:1 to the `retries` column, which has always
  meant this (fact 1), and the name is chosen so the ambiguity cannot survive
  reading the manifest. `attempts: 1` is "no retry".
- **`backoff`** — the base delay, an ISO-8601 duration (`PT10M`). Every other
  duration rbpmn reads is spelled this way (timer specs), and it is validated
  by the same `iso8601::fixed_length_seconds` cycles use, which refuses months
  and years — a backoff whose length depends on where in the calendar it lands
  is not a backoff — and refuses zero and negatives.
- **`multiplier`** — the growth factor, default 3, preserving today's curve
  exactly.

**The multiplier earns its place, and the reason is `multiplier: 1`.** A base
alone can only express a steeper ramp or a shallower one *by moving the whole
curve*; it cannot express a constant gap. "Try every ten minutes, twelve
times" is a policy applications actually want for a dependency that is either
up or down, and `multiplier: 1` is the only way to say it. Gentler-than-3
ramps (1.5, 2) fall out of the same member.

Every member is optional and absent means inherited (D2), so the minimum
useful entry is one key. An entry that sets *nothing* is refused (D5): it is
wiring the author believes is in force and is not.

`multiplier` is a float, and the integer spelling costs nothing: `deploy`
hashes the manifest it *re-serialized*, so `2` and `2.0` both arrive as `2.0`
and hash identically. Worth knowing before anyone adds a normalizing
serializer to fix a problem that is not there — fixture 28 writes `2`.

### D4 — it must not serialize when empty

`deploy` hashes `serde_json::to_value(bindings).to_string()`. A group that
always appeared would change every existing `content_hash` and — because
redeploy-at-startup is a documented, deliberately idempotent pattern —
allocate a new version of every deployed definition in every installation on
first boot after the upgrade. So `#[serde(default, skip_serializing_if =
"RetryPolicies::is_empty")]`, and the same on both inner maps and on every
member of a policy, so an entry writes back the narrowest spelling that
carries its meaning.

The existing `definition_scoped_manifests_serialize_byte_for_byte` proves the
hash cannot move; `an_empty_retry_group_does_not_reach_the_hashed_manifest`
says it in this feature's own words.

### D5 — `retry-policy-binds-task` ⁺ (error)

One rule id, four clauses, on the `config-binds-task` pattern — a modeller
with several defects has several things to fix and is told all of them:

- a `by_element` key that is not in the process at all;
- a `by_element` key that is in the process but is not a **service task**
  (user tasks do not fail through a handler; a business-rule task is decided
  by the engine inside the step transaction and has no work item to spend a
  budget on);
- a `by_topic` key that no service task in this process resolves to;
- a member out of range: `attempts` below 1 or above 1000, a `backoff` that is
  not a positive fixed-length ISO-8601 duration *or is longer than the
  ten-year ceiling* (D7), a `multiplier` below 1 or above 10 — and an entry
  that sets no member at all.

Error severity, for `config`'s reason: this group has no default in the sense
that matters. A stale key in `topics` overrides nothing because a topic *has*
a default; a retry policy that binds nothing is an operator's belief about how
a step behaves that the engine does not share, and it will be discovered
during the incident it was written to prevent.

Reported through `check_deployable`, so the editor's L2 pane, the playground,
`deploy` and `check_active_definitions` all get it from one implementation.
Like `config-binds-task` it is held back from the compile gate and appended
after, so a manifest defect never hides `unresolved-topic`.

The bounds are the two that are load-bearing rather than tidy. `attempts`
lands in an `int` column, and a manifest-driven overflow would fail an insert
*inside a step transaction*. `multiplier` feeds `power(mult, 20)` in SQL,
where a float8 overflow raises rather than saturating — it would abort the
very transaction recording the failure and strand the item. Ten leaves a
margin of ~270 orders of magnitude; it is also the point past which the
exponent cap is reached in three failures and every later gap is the ceiling
anyway.

### D6 — two nullable columns, and NULL means "ask the engine"

`attempts` needs a column because `retries` already is one: the budget is
per-item mutable state that the fail path decrements. `backoff` and
`multiplier` need one because they are read at **fail** time, inside the
`UPDATE` that computes `retry_at`.

```sql
retry_at = clock_timestamp() + make_interval(secs => least(
    coalesce(backoff_base, $3) * power(coalesce(backoff_multiplier, 3),
                                       least(failures, 20)),
    315360000))
```

**This is the one place this round parts company with task-config D6, which
rejected a column for `config` on principle.** The distinction is what
consumes the value. Config is unbounded application JSON read *by a claimant,
in Rust, after the claim* — so a column would be a second copy of an
arbitrarily large fact that only ever travels outward, and the definition
cache serves it for free. The retry curve is two scalars consumed *by the SQL
statement that applies them*, beside `clock_timestamp()` and `power()`.
Resolving it in Rust would mean the statement's inputs came from two places,
and it would put a definition-cache lookup on the fail path for a value the
row can carry in sixteen bytes.

**NULL is not "3 and the engine default baked in".** It means *ask the engine
now*. That keeps every pre-existing row behaving exactly as it does, and it
keeps `retry_backoff` a runtime setting rather than something frozen into rows
at insert time — an operator who lowers the base for an incident window sees
it apply to work already queued, which is the whole reason it is a runtime
setting.

`attempts` is different and deliberately so: it is written at insert as a
concrete number, because it is a *budget being spent*, and a budget that
changed size while it was being spent would make `retries` and `failures`
disagree about the same policy. Where the manifest says nothing the engine
writes `DEFAULT_ATTEMPTS`, whose agreement with the column default from
migration 0001 is asserted by a test rather than by comment.

### D7 — ten years, said in three places, because one is not enough

`least(…, MAX_BACKOFF_SECONDS)` caps the *computed* gap: `make_interval` has a
ceiling and every value feeding the expression is manifest-driven now. It is
unobservable for existing deployments — reaching the exponent cap needs 21
failures, and before this round every item had a budget of 3.

**The cap alone is not a guard, and saying it was is the mistake this section
originally made.** `least` sees `power`'s result, so a row whose
`backoff_multiplier` is large enough raises `value out of range: overflow`
*before* the cap is consulted — inside the very statement recording the
failure, which aborts the transaction, loses the failure, and leaves the item
being retried into the same abort forever. So the ten years is stated three
times, each covering what the others cannot:

- **`retry-policy-binds-task`** refuses a declared base past it. Capping a
  base someone *wrote* would be reinterpreting a manifest, which is the one
  thing this project does not do — the cap exists to keep arithmetic
  representable, not to edit a modeller's intent.
- **CHECK constraints on both columns** (migration 0019) refuse the values
  deploy never saw: a hand-edited row during an incident, a restored dump,
  "any future writer". This is the only place the guard can be total, because
  it is the only place that sees every write.
- **`least` in the fail path** bounds what the arithmetic can produce from
  inputs that are already legal.

`the_columns_refuse_what_the_rule_refuses` reproduces the overflow case rather
than asserting it, so the day someone drops a constraint the test says what it
costs.

### D8 — the columns are published, and the inspector answers the question

Both go on `rbpmn_v_work_item`, **appended after `created_at`** rather than
placed beside `retries`: a view's column order is part of what `select *`
returns, and `create or replace view` only permits additions at the end. The
alternative — an application joining `rbpmn_v_definition.bindings` and
re-implementing element-then-topic-then-engine resolution in SQL — is exactly
the "every application re-derives the rule" failure the published views exist
to prevent.

The inspector's work-item row gains the policy *and* `retry_at`, because
"why has this not retried yet" is answered by when, and the two new fields say
nothing on their own. Read-only, like everything else there.

Not added: anything on `LockedTask` or `WorkItem`. A handler does not decide
its own budget, and a worker that knew its policy could only be tempted to
implement a second one.

### D9 — non-goals

- **No per-instance override, and no operator retry button.** The inspector is
  read-only forever; changing a policy is a deploy (D1).
- **No engine-wide `attempts` setting** to match `retry_backoff`. The manifest
  now says this per element and per topic; a third way to say it would be a
  third place to look when the number is wrong. The asymmetry is the point:
  the engine keeps the fallback that is genuinely installation-shaped (how
  fast this environment retries) and not the one that is process-shaped (how
  many times this step is worth trying).
- **No policy on user tasks or business-rule tasks.** Refused at deploy (D5)
  rather than ignored.

---

## What `spec/` says, having re-read it

`Lease.tla` models `Backoff` and `Retries` as **constants**, and the fail path
as `Fail` (budget left) / `FailFinally` (budget spent). Making both per-item
changes which values an item carries, not the transition structure — the model
is already "one item, one backoff, one budget", and nothing in it reads the
values except `retryAt' = now + Backoff` and the `retries > 0` guard.

Two things the change must therefore preserve, and does:

- **The backoff is positive and finite.** `Deadline == 0..(MaxTime + TTL +
  Backoff)` is the model's whole reason for bounding it; a zero or unbounded
  backoff would put `retryAt` outside the state space (and in the engine,
  outside `make_interval`). D5's duration validation and D7's cap are what
  keep that true now that a manifest supplies the number.
- **The budget is at least one.** `attempts >= 1` keeps `FailFinally`
  reachable and the item non-stranded.

One finding, recorded rather than fixed: the model's `Retries` is *not* the
`retries` column. Its comment says "failures before the instance freezes", and
that is self-consistent — `Retries = N` allows N `Fail` steps and then a
freezing one, i.e. N+1 failed deliveries — but the column has always meant
total attempts. No property counts deliveries, so nothing is wrong; a reader
mapping the constant onto the column one-for-one would be. Now that `attempts`
puts that number in a hand-written manifest, `Lease.tla` carries a comment
saying which is which. The constant is left alone: aligning it would mean
bumping `Retries` in five configs, four of which exist to produce a *specific*
counterexample.

No new lock enters the order, `guard_lease` is untouched, and nothing here
runs in the scheduler's claim path.

---

## Slices

### Slice 1 — the manifest and the verdict (no execution)

Linter rules first, with fixtures, then execution — as always.

- `RetryPolicy`, `RetryPolicies`, `Bindings::retries` with D4's serialization,
  the fluent builders, and `Bindings::retry_policy(element)` doing D2's
  resolution.
- `rule::RETRY_POLICY_BINDS_TASK` + `CATALOGUE` entry.
- `retry_policies` in `check_deployable`, beside `config_bindings`.
- Unit tests in `check.rs`'s shape (an L2 rule cannot have a corpus fixture).
- README manifest row, `docs/rules.md`, editor `parseManifest`.

Owes: `cargo test`, `just lint`, `just parity`, `just ui`, `just ui-test`.

### Slice 2 — execution

- Migration 0019: two nullable columns, `create or replace view`.
- The insert binds `retries`, `backoff_base`, `backoff_multiplier`.
- The fail path's `coalesce`s and D7's cap.
- `rbpmn_v_work_item` and the inspector.

The test that earns the feature: an element-scoped policy of 4 attempts at
`PT2S` with multiplier 2 produces gaps of 2s, 4s, 8s and then an incident,
while a second definition in the same engine keeps the default curve.

Owes: `cargo test` (needs Postgres), `just lint`, `just tla`.

---

## Test plan

| What | Where |
|---|---|
| An empty retry group does not reach the hashed manifest | `compile.rs` |
| A policy round-trips, narrowest spelling, unknown member refused | `compile.rs` |
| Element beats topic, per member, and topic beats nothing | `compile.rs` |
| Each of D5's rejections | `check.rs` |
| A retry-policy error does not hide the compile stage | `check.rs` |
| `DEFAULT_ATTEMPTS` equals the column default | `engine.rs` |
| An element policy is honoured: attempts *and* widening gaps | `engine.rs` |
| A topic policy covers the elements on that topic | `engine.rs` |
| No policy is byte-for-byte today's behaviour, same version on redeploy | `engine.rs` |
| A per-element policy and the engine default coexist in one engine | `engine.rs` |
| The view's shape, with the two columns appended | `engine.rs` |
| The columns refuse what the rule refuses, overflow included | `engine.rs` |
| A base past the ten-year ceiling is refused, not capped | `check.rs` |
| Native and WASM agree over the corpus with its manifests | `just parity` |
| The editor round-trips a retry manifest byte for byte | `just ui-test` |

---

## Known warts, stated up front

- **Two numbers now live in two places.** `backoff_base` on the row and the
  policy in the manifest are the same fact, and D6 argues why that is right
  here and wrong for config. The honest cost is that an operator editing a
  manifest does not change the queued items already carrying its predecessor's
  numbers — which is correct (the instance is pinned) and still surprising.
- **The gap between `attempts` and `Retries` in `Lease.tla`** is documented,
  not closed. See above.
- **A policy cannot make an item retry *sooner* than the engine base after the
  fact.** There is no path that recomputes `retry_at` for a parked item; the
  policy applies from the failure that follows a redeploy, and only to
  instances started against the new version.
- **The retry rules are compared between native and WASM only where they stay
  silent.** No sidecar in the corpus is deliberately wrong, so `just parity`
  proves the two builds agree that a *valid* manifest is valid, and nothing
  about the four refusal clauses or their message text. This is the gap
  task-config recorded and declined to close — `expect-diagnostics` is a
  comment in the `.bpmn` and reads L1 only — and this round declines it again
  rather than inventing a corpus convention on the way past. The messages are
  covered by `check.rs`'s unit tests, in one build.
