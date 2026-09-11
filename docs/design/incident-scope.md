# Incident scope — design round

**Status: one decision taken.** D1 — the catch-all error boundary — is
decided and owed as a slice. The freeze stays instance-wide (D2), and the way
out of an incident is a repair API rather than a narrower freeze (D3);
everything from D4 down is the shape that round will start from, not a
commitment to build it now. The alternatives that were weighed and refused
are at the bottom, briefly, because the reasoning for not taking them is the
part worth keeping.

This round covers **what an incident freezes, and how an instance gets out of
one**. It was read against `crates/rbpmn-core/src/step.rs` (`freeze`,
`RaiseError`, the completion test), `compile.rs` (`error_boundary`,
`ExecKind::ErrorBoundary`), `crates/rbpmn-model/src/lint/mod.rs` (the
`errorRef` requirement), `crates/rbpmn-engine/src/lib.rs` (`CLAIMABLE`,
`IN_PROGRESS`), `runtime.rs` (the three `IncidentOpen` gates, `correlate`'s
resolution, `fail_work_item_in_tx`, token persistence), `scheduler.rs`,
`retention.rs`, `worker.rs`, `ui/src/inspector/`, `spec/Lease.tla`, and
`docs/design/boundary-messages.md` §2.3, which supplies the proof the
question turns on.

---

## The motivating case

A host activity waits on a message the rest of the process depends on. A
non-interrupting boundary on it spawns a side path that runs an auxiliary
activity — a notification, a periodic report, anything the outcome does not
turn on. That handler fails on something outside the process and keeps
failing, the activity burns its retry budget, `RaiseError` finds no matching
boundary (`step.rs:346`) and freezes the instance. The awaited message now
answers 404, every other branch stops with it, and the process can never
finish — because an activity nothing depended on could not run.

The side path is the one part of a model whose entire point is that it is not
essential, and it is the one part rbpmn can *prove* inessential:
`boundary-side-path` refuses to deploy it unless it merges into nothing.

## What the freeze does — five gates, none scoped to the failing branch

One assignment, `state.status = InstanceStatus::Failed` (`step.rs:1419`),
turns off five things at once:

| Gate | Where | Effect on the *other* branches |
|---|---|---|
| Claim | `CLAIMABLE`, `lib.rs:379` | no worker can claim any item in the instance |
| Complete / fail | `runtime.rs:214, 340, 437` → `IncidentOpen` → HTTP 409 | a worker whose handler already ran cannot record it |
| Correlate | `runtime.rs:314` (`and i.status = 'active'`) | every message to the instance answers the typed no-match |
| Timers | `scheduler.rs:146` | every armed timer stops; an SLA escalation that would have fired does not |
| Retention | `retention.rs:310`, `:51` | `failed` is never swept at any age; only `delete_instance` removes it |

Two consequences of that are not visible in the status column.

**A sibling worker loses work it has already done.** `IN_PROGRESS`
(`lib.rs:392`) deliberately reads a leased item on a frozen instance as still
in progress, "because a worker really is holding it". When that handler
returns, `complete_task` answers `IncidentOpen` and the worker keeps the
lease rather than releasing it (`worker.rs:242`) — correctly, because
releasing would re-run the side effect. So the effect happened and is
unrecordable until an operator intervenes.

**Inert arms are still rows.** A frozen instance keeps its subscriptions and
timers by design: they are what a repair resumes from. They are excluded from
delivery and firing by instance status, never by a column, which is why the
inspector has to derive inertness rather than read it (see *What the operator
sees*).

## Three reasons the freeze is instance-wide, unequally load-bearing

**1. The variable document.** Variables are one opaque JSONB document with no
branch-local scoping. A token left running past a sibling's failure, or
resumed after one, works against a document the other branches have moved.
This is not hypothetical for side paths specifically: the side path in
`accept/40-late-fee-cycle.bpmn` exists *to* patch variables the main flow
reads. A side
path is control-flow-disjoint and data-coupled, and only the first half is
checkable.

**2. Token conservation, for the repair the freeze exists to make possible.**
`freeze`'s own contract is that frozen means *nothing advances and nothing
vanishes*. What that buys is **frozen evidence**: the state an operator
inspects is the state at the instant of failure, and the document a repair
resumes into is the document that failed. D3 is where that is collected.

**3. The claim gate is cheap.** One conjunct, index-friendly, provably total,
and byte-identical to migration 0015's copy — held together by
`the_view_and_the_claim_predicate_cannot_drift`. This is mechanism, not
principle, and the only one of the three that would be cheap to make
per-branch.

---

## Decisions

### D1 — the catch-all error boundary (decided)

An error boundary with no `errorRef` catches any error. That is standard
BPMN; the parse already carries it (`model.rs:240`, `error_ref: Option<Id>`)
and one L1 rule refuses it (`lint/mod.rs:689`). The rule relaxes: an **absent**
`errorRef` is a catch-all; a **present but unresolvable** one stays the error
it is today.

This is the answer to the motivating case, and it is a modelling answer
rather than an engine one: the author says on the diagram that a failure here
is survivable, and a reviewer sees them say it. Nothing about the freeze
changes — a model that does not draw one behaves exactly as now.

Four things it must get right:

- **It catches the codeless failure.** `RaiseError` today only looks for a
  boundary when the failure carries a code (`step.rs:309`), so a handler
  that fails without one can never be caught. That is the most common shape
  of the unanticipated failure this rule exists for, so the catch-all is
  matched outside that condition, not inside it.
- **Most specific wins.** An exact code match is preferred over the catch-all
  at the same host, and the outward walk through enclosing scopes is
  unchanged: each step out tries exact, then catch-all.
- **One per host.** Two catch-alls on one activity is a new L1 error; there
  is no order to break the tie with, and picking one would be a guess.
- **`ExecKind::ErrorBoundary { code }` becomes `Option<String>`**, so "catches
  anything" is a shape rather than a sentinel string a model could collide
  with.

`boundary-side-path` needs no change: its closure already includes the
boundaries of activities on the path, so a catch-all on a side-path task must
end inside the side path like everything else there.

### D2 — `side-path-failure-escapes` ⁺ (warning), shipping *with* D1

A failure on a side path that no catch-all *on the side path* contains
escapes it: with nothing further out it freezes the whole instance, and with
a catch-all further out it tears down the scope around the host — the very
flow the side path existed to leave alone. That is exactly the surprise the
motivating case is made of, and a warning is the honest way to say it: it is
legal BPMN, the standalone linter serves models targeting other engines, and
the fix is now drawable. The name says *escapes* rather than *freezes*
because the second outcome is not a freeze and is no better.

**Ordering matters**, the same way the expression-timer round records it: the
warning ships with the capability, never before it, or it names a fix that
does not exist.

### D3 — the freeze stays instance-wide; the way out is repair

The freeze's real cost is not its width but its permanence: an instance
frozen for ninety seconds is an incident, an instance frozen forever is a
write-off. Today the only exit is `delete_instance`, which destroys the
evidence.

So the direction is a repair API, and the freeze is what makes one sound:

- **The variable document is the one that failed.** Nothing patched it,
  because nothing ran. A repair resumes into exactly the world its operator
  is looking at.
- **Repair races nothing.** The freeze is a global barrier — no step, no
  delivery, no timer, no claim — so two operators repairing two tokens
  serialize on the instance lock and contend with nothing else.

Narrowing the freeze does not buy a cheaper repair. It buys a harder one, and
it does nothing at all for a main-flow incident, which is the case a repair
has to handle anyway.

### D4 — a repair is a `Command`, never a state edit

Setting `status` back to `active` is not a weak repair, it is a broken one.
The failing token stays parked at `Incident` and its item stays `failed`, so
nothing claims it, nothing advances it, and every query in the read surface
reports the instance as healthy. That is a state **the core cannot produce by
itself**, which is what makes it wrong: a stuck instance wearing an active
status is worse than a frozen one, which at least tells the truth.

So every repair is a command whose effects go through `Advancer` like any
other, which keeps every reachable state one normal execution could have
reached, and keeps the history re-derivable — `chaos.rs` replays through the
pure core, and an `UPDATE` would put the instance beyond it.

### D5 — four dispositions, and a fifth verb for the instance

| Disposition | What it does | For |
|---|---|---|
| **Retry** `{ patch?, reason }` | patch the variables, then re-enter the node as if for the first time | the world was fixed, or the data was wrong and the patch fixes it |
| **Advance** `{ result, reason }` | treat the node as completed with an operator-supplied result and leave along its outgoing flow | "the step was carried out by hand"; and the decision case, where `result` is the answer the projection could not produce |
| **Divert** `{ code, reason }` | raise `code` at the node so a boundary catches it and the model handles the failure | the author drew a handling path; refused loudly when nothing matches, because diverting into nothing is what this engine never does |
| **Abandon** `{ reason }` | consume the token | refused unless removing it is provably safe — the side-path proof, or emptying its scope |

Plus **`abandon_instance`**, ending it as `Terminated`. Not a convenience:
`failed` is never swept at any age, so an unrepairable incident is immortal
today, and `Terminated` is `DUE`, so an abandoned instance retires normally
with its history intact.

**Repair never reopens a closed item; it creates a new one.** `Retry`
re-enters the node, minting a fresh work item with the budget the manifest
declares, and the `failed` row stays closed forever. This dissolves by
construction the hazard `freeze` warns about — *a repair API that clears the
incident would hand a worker an item whose token is parked at
`WaitKind::Incident`* — and a worker holding a stale item id gets the
ordinary `AlreadyClosed`.

### D6 — the instance goes active because nothing is parked at an incident

There is deliberately no `resume` verb. Status is derived, exactly as
completion is (`step.rs:564`): the instance returns to `Active` when no token
remains at `Incident`. An operator cannot set it, and a partially repaired
instance stays frozen, so there is no half-open window.

One consequence to state rather than discover: during a partial repair a
repaired token advances immediately and may create an `available` work item
on a still-`failed` instance. The claim gate is what makes that safe. It
inverts an invariant `freeze` currently maintains by closing such items, and
it is relied on here deliberately rather than inherited.

The alternative — record each token's disposition and apply them together at
the end — is refused: stored intent is a state the core cannot produce, which
is the thing D4 is organised around.

### D7 — cause and collateral are different states

`freeze` parks more than the failing token at `Incident`: siblings still in
transit park at their target nodes, and any token at `WaitKind::Decision` is
converted. Those are bystanders, and an operator must not hand-repair five of
them.

The state cannot currently tell them apart — `IncidentRaised` names the
failing element, but the token set does not, and matching by node is
ambiguous across scopes. So `WaitKind` gains a second variant (`Halted`) for
the bystanders, leaving `Incident` meaning the cause. `wait_kind` is a text
column behind a CHECK (`runtime.rs:1266`), so this is a migration widening
the CHECK rather than a serde break, and rows written before it read as
`Incident`, which is the safe reading.

Collateral then resumes with the last cause: re-entering a bystander's target
node is what would have happened anyway, and re-asking a bystander's decision
is sound because the document has not moved. Tokens in ordinary wait states
were never converted — they are inert through instance status alone and
become live again with no work.

### D8 — resume is a re-arm

A frozen instance keeps its armed timers with absolute `due_at`, excluded
from firing by status alone. Resume one frozen for three weeks and every
deadline inside that window is due at once; cycles are worse, because a cycle
re-arms from its **previous due**, so a weekly reminder fires three times in
succession, each re-arm landing in the past.

The policy is the rule the cycle design already states, applied to resume as
if it were an arm — *the first due is the first occurrence at or after the
arm, no catch-up*. Relative deadlines shift by the outage, cycles
resynchronise to the next occurrence after resume, and absolute `timeDate`
deadlines fire once, because that deadline genuinely passed. Reusing a
decided rule beats inventing one, and the outage is derivable from
`incident-raised`'s timestamp.

### D9 — what repair does not cover

- **Model bugs.** A wrong topic or an unresolvable timer expression is fixed
  in a new definition version, and a running instance pins its version. That
  is the instance migration API, and keeping it out is what keeps this one
  small.
- **The outage window.** Repair needs a human: a dependency that vanishes at
  02:00 on a Sunday leaves the instance frozen until Monday.

That second point is why D1 and D3 are complements rather than alternatives.
The catch-all is the author saying in advance that a failure is survivable;
repair is an operator deciding about one nobody anticipated. D1 is much the
cheaper, which is why it goes first.

### D10 — non-goals

- **No repair button.** The inspector is read-only, and a repair API is
  exactly the pressure the standing rule anticipates; the element pane is
  where it will arrive. Diagnosing which dispositions are legal for a token
  is a read. Offering the button is not.
- **No authenticated actor.** The engine does not know identities; the
  embedding application does. `reason` is an opaque string that travels with
  the evidence, which is what makes the history an audit trail rather than a
  log.

---

## Rejected alternatives

**Quarantine the failing token, instance stays active** (the job engines'
shape). Refused on cost and on soundness. It needs a per-branch claim gate in
`CLAIMABLE` and its migration-0015 twin, a per-token correlation filter, a
sixth instance status — widening an enum the published views expose, which
the "never repurposed" rule does not cover — retention handling, and an
answer to instance completion, since a quarantined token is never consumed
and the instance therefore never becomes terminal or sweepable. And it
forfeits both things D3 collects: the repair would resume into a document
that moved, and it would race the live instance including its completion.

**Discard the failing side token automatically.** Mechanically nearly free —
`cancel_attachments` and the work-item close already do it — and refused
because it is the engine guessing that a side effect did not matter. The same
outcome is available as `Abandon` (D5), with an operator behind it and a
reason recorded, which is the difference between a decision and a guess.

**Freeze the enclosing scope.** A scope is not the blast radius: the variable
document is instance-wide, so this gets the data question exactly as wrong as
quarantine while also being unable to express the root-scope case, which is
where side paths usually live.

**Failure disposition in the manifest** (`Bindings::on_failure`). The form
either of the two above would take if either were taken. Held, not designed:
there is no live implementation to generalise over, and D1 plus D3 covers the
cases that motivated it.

**Accept intake while frozen** — `correlate` buffering into a frozen instance
instead of answering 404. Tempting, because it keeps a frozen instance's
callers working without touching token semantics at all. Refused here: it
trades a loud 404 for a silent deferral, and a message buffered against an
instance later abandoned is accepted and never acted on. Buffering and
dead-lettering are owed by the cross-definition messaging round, and should
be answered once.

---

## What `spec/` says, having re-read it

`Lease.tla` already writes the cost down, in `NeverStranded`'s own comment
(`spec/Lease.tla:458`): the engine does have stranded items — a sibling
branch's open task on an instance this item froze — and a one-item model
cannot express it.

That gap is worth closing whether or not repair is built, because it turns
the price of D2 from an argument into a check. Two items, `active` per
instance as today, and

```
SiblingStrandedByAFreeze ==
    Open(i2) => (Claimable(i2) \/ \E w \in Workers : Completable(w, i2))
```

as a config **expected to fail**, matched to the counterexample it
demonstrates — the motivating case, in the idiom the other twelve failing
configs already use.

What repair adds later: a transition out of `active = FALSE`, which no model
has today because the freeze is currently terminal. Its own module, because
the property that matters is that a repaired token never resumes into an
instance that has meanwhile completed — the same shape as `BoundaryExit`'s
*a late call of either verb is answered typed, never stepped*. `LockOrder`
needs nothing at either arity: no new lock enters the order.

## The guarantees this preserves

1. An incident is **total**: after it, nothing about the instance changes
   except by an operator.
2. **Frozen evidence**: what is inspected is the state at the moment of
   failure, and it is the state a repair resumes into.
3. **Token conservation**: nothing is lost, so a repair can be total and has
   exactly one shape to resume from.
4. **One gate**: `i.status = 'active'`, in five places, each provably
   complete.
5. No instance is ever half-done without saying so.

D1 spends none of them. The rejected alternatives spend (2) and (3).

## What the operator sees

```
                        ┌── (msg) REPLY ───────────────┐
start → prepare → [ await_reply ] ─────────────────────→ record → done
                        ⊙ R/P7D  (non-interrupting)
                        └→ notify → notified             ← side path
```

**Without a catch-all.** Status chip `failed`. A solid red mark on `notify`,
and every other arm in the instance — the `REPLY` subscription, the cycle
that armed the side path — drawn **inert**: dashed, muted, and titled with
the reason (`awaiting REPLY (key K-1) — inert: the instance is frozen on an
incident`). Inertness is derived from instance
status in `ui/src/inspector/marks.js`, because there is no column for it: a
frozen instance's rows are real and nothing will act on them, and a diagram
that draws them like live ones claims the failure is one element wide when it
is the whole instance. The marks that record the failure — the incident token
and the failed work item — are never muted; they are the reason, not an arm.

**With one (D1).** The catch-all on `notify` takes the side path to its own
end, `notified` records that it did not run, `REPLY` correlates, and the
instance completes.

## Slices

**Slice 1 — the catch-all (D1 + D2).** Fixtures first, both directions: an
absent `errorRef` accepted and matched, a present-but-unresolvable one still
refused, two catch-alls on one host refused, a codeless failure caught, an
exact code preferred over the catch-all at the same host, and the outward
walk trying both at each step. Then `ExecKind::ErrorBoundary`'s
`Option<String>`, `error_boundary`'s match order, `RaiseError`'s codeless
path, and the `side-path-failure-escapes` warning with its own fixtures.
Editor: the pane already
produces one — clearing the *error code* field writes `errorRef: undefined`
(`properties.js:414`), so today it authors a model the linter then refuses.
D1 makes that reachable rather than broken; the field's hint gains the second
half of what it does, and the round trip is worth a test. Owes `just ui`,
`just parity` and `docs/rules.md`.

**Slice 2 — the failing `Lease` config.** Independent of everything else, and
cheap.

**Slice 3 — repair.** Its own round, starting from D4–D9.

## Test plan

- **Fixtures** for every clause of D1 above, in
  `crates/rbpmn-model/tests/fixtures/{accept,reject}/`, with the expected
  diagnostics embedded.
- **Scenarios** for the execution half: a codeless failure caught by a
  catch-all; an exact code preferred over one; a catch-all on an enclosing
  subprocess catching a failure the inner activity's boundaries did not; and
  the motivating case end to end — side-path failure caught, host completes,
  instance completes.
- **`just parity`**, because the rule is new surface on both exports.
- **UI**: `marksFor` is covered under node (`ui/test/marks.test.mjs`); the
  served e2e half already asserts mark counts on an active instance.

## Known warts, stated up front

- **D1 relocates the question, it does not dissolve it.** A catch-all must be
  drawn on every activity of every side path, and a model that forgets one
  behaves exactly as today. That is what makes D2 a warning rather than
  optional paperwork — and if that warning proves noisy, the answer is to
  keep it scoped to side paths, not to soften it.
- **A catch-all is a blunt instrument, and that is the point.** It cannot
  distinguish a missing dependency from a misconfigured handler, so a model
  that wants those handled differently still needs coded errors and a handler
  that returns them. The catch-all is the floor,
  not the pattern to reach for first.
- **The retry budget still runs first.** Everything here is downstream of
  exhaustion, so the window is the budget's length whichever route is taken.
  Correct — a transient failure must not abandon a path — but it means none
  of this shortens the outage, only what happens at the end of it.
- **`Divert` lets an operator override the error taxonomy** the model's
  author wrote. Practical, and slightly against the grain; it is in D5
  because the alternative is `Advance` with a hand-written result, which is
  less honest about what happened.
- **The data half stays unproven.** Nothing here gives the engine a reason to
  believe the variable document is intact after a branch failed partway
  through writing it. The freeze sidesteps it by stopping everything; repair
  sidesteps it by resuming into the same document. Neither checks it, and no
  option on the table could.
