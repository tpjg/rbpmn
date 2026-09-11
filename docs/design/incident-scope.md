# Incident scope — design round

**Status: D1 and D2 shipped, and the price of D3 is checked. D4–D13, repair,
are decided, and slices 3–6 build them.** The catch-all error boundary
and the warning that ships with it are in. The freeze stays instance-wide, and
the way out of an incident is a repair API rather than a narrower freeze (D3).
The alternatives that were weighed and refused are at the bottom, briefly,
because the reasoning for not taking them is the part worth keeping.

This round covers **what an incident freezes, and how an instance gets out of
one**. It was read against `crates/rbpmn-core/src/step.rs` (`freeze`,
`RaiseError`, the completion test), `compile.rs` (`error_boundary`,
`ExecKind::ErrorBoundary`), `crates/rbpmn-model/src/lint/mod.rs` (the
`errorRef` requirement), `crates/rbpmn-engine/src/lib.rs` (`CLAIMABLE`,
`IN_PROGRESS`), `runtime.rs` (the three `IncidentOpen` gates, `correlate`'s
resolution, `fail_work_item_in_tx`, token persistence), `scheduler.rs`,
`retention.rs`, `worker.rs`, `ui/src/inspector/`, `spec/Lease.tla`, and
`docs/design/boundary-messages.md` §2.3, which supplies the proof the
question turns on. The repair decisions were read against every `freeze`
call site, `persist_step` and `insert_timer`, `step_answering_decisions`,
`worker.rs`'s kept lease, and the replay harness's `commands_from`.

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
BPMN, and the parse carries it (`model.rs:240`, `error_ref: Option<Id>`).
Under `bpmn-structure`, an **absent** `errorRef` is a catch-all and a
**present but unresolvable** one is an error: a typo must never read as
"catch everything".

This is the answer to the motivating case, and it is a modelling answer
rather than an engine one: the author says on the diagram that a failure here
is survivable, and a reviewer sees them say it. Nothing about the freeze
changes — a model that does not draw one behaves exactly as now.

Five things it must get right:

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

- **A round trip must not be what makes one.** bpmn-moddle reports a
  reference that resolves to nothing only as an import warning and drops it
  on export. With an absent `errorRef` meaning *catch everything*, a mistyped
  reference passed through any bpmn-js modeler comes back as a catch-all, and
  nothing in the XML is left to say it was a typo. The linter cannot see what
  is no longer there, so the tools that round-trip must: `just fixtures-di`
  refuses anything moddle warns about, and the editor reports a dropped
  `errorRef` as an error on its boundary until the boundary has a code again — carried
  across the editor's re-imports of its own serialization (the XML box, a
  theme change), which moddle wrote and which no longer hold the reference to
  find (`ui/src/editor/dropped-refs.js`). bpmnlint shares the blind spot — its
  rules see only the tree moddle built — so the plugin lists that fixture
  beside `duplicate-id` as something only deploy can catch. What no tool here
  can reach is a third-party modeler that saves after importing: that has
  made the catch-all real, and nothing downstream can tell it from one drawn
  on purpose.

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

It lands on the activity that can fail: a service or user task directly on
the path, or — once, naming what escapes it — a subprocess on the path whose
body lets a failure through, because a catch-all on the subprocess is the one
fix for all of it. When side paths nest, a node of the inner one is
reported once, under the innermost boundary, whose activations are what run
it. A user task cannot carry an error boundary, so its message
says to wrap it. It speaks only for well-formed side paths, since a
`boundary-side-path` error is enough on its own; and only for the error class:
decision, timer and correlation incidents are not errors, no boundary catches
them, and a warning there could name no remedy.

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
- **Repair races nothing but itself.** The freeze is a global barrier — no
  step, no delivery, no timer, no claim — so a repair contends only with
  another repair of the same incident; the instance lock serializes those,
  and D9 gives the loser a typed answer.

Narrowing the freeze does not buy a cheaper repair. It buys a harder one, and
it does nothing at all for a main-flow incident, which is the case a repair
has to handle anyway.

### What repair has to fit — four things in the code

**1. There is only ever one incident.** Every freeze has exactly one token as
its cause; whatever else it parks — a move still in flight, a decision still
pending — is collateral. A frozen instance takes no step until a repair, and
a repair is one step: it lands the instance active or freezes it again with a
new cause. So a repair names the instance and its incident, never a token —
token ids are internal, and choosing among five of them is what D7 spares an
operator.

**2. Where a token failed is not always where it resumes.** `freeze` parks
the cause at the element that failed, which is what inspection must show.
Resuming means entering an element again, and for two failures that is
another one:

| Failure | Parked at | Resumes at |
|---|---|---|
| a work item's error, uncaught | the task | the task |
| a decision with no usable answer | the business-rule task | the same task |
| a catch that cannot arm — deadline, key, duplicate | the catch | the catch |
| a host's boundary that cannot arm, on entry | the boundary | its host |
| an event-gateway alternative that cannot arm | the alternative | the gateway |

Both redirections are static — a boundary has one host, and lint gives an
event-gateway target exactly one incoming flow — so the cause needs no new
state. The resume point is *R* below.

**3. Collateral comes in two shapes, and the freeze keeps neither's resume
point.** A move in flight has not entered its node: it resumes by entering
it, and a parallel join needs the flow it arrived on, which the freeze drops.
A pending decision has entered its node (`element-started`,
`decision-requested`): it resumes by asking again, and must not start the
element a second time. Both park as `Incident`, indistinguishable from the
cause and from each other.

**4. One freeze makes something vanish.** A non-interrupting message boundary
re-arms on every delivery, evaluating its key against the document the
delivery just patched. When that fails — the patch left the key unusable, or
moved it onto another open `(message, key)` — `freeze` takes the **host's**
token: it cancels the host's open work item, perhaps leased with its handler
running, and tears down a subprocess host's live body. It is the one freeze
that breaks token conservation (guarantee 3), and a repair at the host would
re-run work already under way. D11 removes it.

### D4 — a repair is a `Command`, never a state edit

Setting `status` back to `active` is not a weak repair, it is a broken one.
The failing token stays parked at `Incident` and its item stays `failed`, so
nothing claims it, nothing advances it, and every query in the read surface
reports the instance as healthy. That is a state **the core cannot produce by
itself**, which is what makes it wrong: a stuck instance wearing an active
status is worse than a frozen one, which at least tells the truth.

So a repair is `Command::Repair { incident, disposition, reason }`, stepped
through `Advancer` like any other command. Every state it reaches is one a
normal execution could have reached, and the history re-derives: its event,
`incident-repaired`, carries what `commands_from` needs to rebuild it, as
`work-item-failed` does for `RaiseError`, so `chaos.rs` replays a repaired
instance through the pure core like any other. `reason` is required, opaque
to the engine, and travels in the event's payload, outside the stable
`Display` line.

### D5 — five dispositions

Each acts at the resume point *R*. Every refusal is typed and comes before
any mutation.

| Disposition | At *R* | Refused when |
|---|---|---|
| **Retry** `{ patch? }` | merge the patch, then enter *R* as if for the first time: a new work item with the manifest's retry budget, the decision asked again, a deadline or key resolved against the patched document | never — failing again is a new incident |
| **Advance** `{ patch? }` or `{ answer }` | complete *R* and leave along its outgoing flow: a task, catch or subprocess as though it had finished, `patch` merged; a business-rule task with `answer` written by replacement, as a decision's answer always is | *R* is an event-based gateway, which has no single way on; `patch` for a decision, `answer` for anything else |
| **Divert** `{ code? }` | raise the error at *R* and walk outward exactly as `RaiseError` does, exact code before catch-all at each host | nothing catches it — diverting into nothing is what this engine never does |
| **Abandon** | consume the token as though it reached an end event: its scope completes if it was the last, and the parent moves on | a join could be left waiting for it. Allowed on a side path — nothing there joins, because `boundary-side-path` ends it in nothing and refuses a parallel block on it — and for the last token of its scope |
| **Abandon instance** | end the instance as `Terminated`, exactly as a terminate end at the root does | never, on a frozen instance |

`Advance` takes `patch` or `answer`, never one "result": merge and
replacement are the engine's two write semantics, a decision's answer needs
the second, and `docs/dmn.md` records the bugs the first produced there.

**Abandon instance** is not a convenience: `failed` is never swept at any
age, so an unrepairable incident is immortal today, and `Terminated` is
`DUE`, so an abandoned instance retires normally with its history intact. It
ends a frozen instance only; ending a running one is its own question (what a
lease holder is told, for one).

**Repair never reopens a closed item; it creates a new one.** The `failed`
row stays closed, and a worker holding its id gets the ordinary
`AlreadyClosed`. That dissolves by construction the hazard `freeze` warns
about — a repair that cleared the incident would hand a worker an item whose
token is parked at `Incident`.

### D6 — the instance goes active because nothing is parked at an incident

There is no `resume` verb. Status is derived, as completion is: an instance
is `Failed` exactly when one token is at `Incident`, and `Halted` tokens
exist only on a failed instance — invariants the explorer and the fsck both
check. A repair sets the instance `Active` before its moves run, so the
ordinary tests apply after it: an Abandon that empties the root completes the
instance, and a Retry that fails again freezes it under a new incident. There
is no half-repaired state; a repair lands or re-freezes within its step.

A repair that re-freezes can leave a resumed sibling's fresh work item
`available` on a `failed` instance. That shape exists already — a parallel
sibling that parked at a task before another branch froze — and the claim
gate is what makes it safe.

The alternative — record each token's disposition and apply them together
at the end — is refused: stored intent is a state the core cannot produce,
which is the thing D4 is organised around.

### D7 — cause and collateral are different states

`WaitKind::Incident` means the cause and nothing else. Collateral becomes
`WaitKind::Halted`, carrying its resume point (finding 3):

- **`Halt::InFlight { via }`** — a move that had not entered its node. It
  resumes by entering, on the flow it was on.
- **`Halt::AwaitingDecision`** — a decision that was pending. It resumes by
  asking again; the element has already started.

Collateral resumes after the cause, in token order, whatever the
disposition: re-entering a bystander's target node is what would have
happened anyway, and re-asking its decision is sound because the document
has not moved. Tokens in ordinary wait states were never converted — they are
inert through instance status alone and become live again with no work.

`wait_kind` is a text column behind a CHECK, so this is a migration adding
`halted` (its flow in `arrived_via`) and `halted_decision`, not a serde
break. Rows written before it read as `incident`, the safe reading. An
instance frozen before the migration with more than one `incident` token
cannot tell its cause from its collateral, so a repair refuses it, typed;
abandoning it still works.

### D8 — resume is a re-arm

A frozen instance keeps its armed timers with absolute `due_at`, excluded
from firing by status alone. Resume one frozen for three weeks and every
deadline inside that window is due at once; cycles are worse, because a
cycle re-arms from its **previous due**, so a weekly reminder fires three
times in succession, each re-arm landing in the past.

So the instance's clock stops while it is frozen:

- a **duration** moves by the outage, keeping exactly the time it had left;
- a **cycle** steps along its own grid to the first occurrence at or after
  the resume — the re-arm rule the cycle design already states, missed
  occurrences skipped rather than replayed, which costs a bounded `R<n>`
  nothing because it counts fires;
- a **`timeDate`** keeps its instant and fires once if it passed, because
  that deadline genuinely did.

The core owns no instants — `due_at` has always been the projection's — so
this lives in `persist_step`: every repair step first re-arms the instance's
timers by these rules, measuring the outage from `frozen_at`, then writes
the step's own. `frozen_at` is a column each freeze sets: runtime truth in a
row, not a read of the event log. A repair that re-freezes has already moved
the timers past the outage behind it, so the next moves them only by the
outage after. The core's timer state never changes, so replay needs nothing.

Work items keep their columns: a retry backoff running when the instance
froze has usually elapsed by the resume, and a lease still held is completed
or extended as before.

### D9 — every incident has a number

Two repairs of one incident serialize on the instance lock, and the second
must not land on whatever the first left behind. That is not only two
operators: an HTTP client whose repair committed but whose response was lost
sends it again, and if the Retry it asked for failed again at the same
element, the resend would repair the new incident as though it were the old
one — or, as an abandon, terminate an instance someone has since repaired.
`release_task` shipped that exact bug with its lease, and an epoch fixed it.

So each freeze mints the next incident number — a per-instance counter
persisted beside the other allocators, carried in `incident-raised`'s
payload — and a repair names the incident it repairs. Any other number, or an
instance no longer frozen, is answered `IncidentNotOpen` (409, carrying the
open incident if there is one) before the core mutates anything. An instance
frozen before the migration holds incident 0.

### D10 — the surfaces

- **`Engine::repair(instance, incident, disposition, reason)`** and its
  `_in_tx` twin, returning what the step did: landed, completed, terminated,
  or frozen again under a new number.
- **`POST /v1/instances/{id}/repair`**, the same four fields as JSON.
- **What a repair would do is a read.** `rbpmn_core` answers, for the open
  incident, its number, *R*, and each disposition allowed or refused with the
  reason; `inspect_instance` carries it and the inspector shows it as text.
  Offering the button is not a read (D13).

### D11 — a delivery that would leave its boundary unable to re-arm is refused

Finding 4 is an incident with its caller standing next to it. The key a
non-interrupting boundary re-arms with comes from the document *after* the
delivery's own patch, and the core can resolve it before touching anything.
When it will not re-arm, the delivery is refused the way `correlate` already
refuses what it cannot step — typed, naming the key and why — and nothing
changes: the subscription stays open, the host keeps waiting, the patch is
not applied. No freeze then ever takes a waiting host, and the scope a
freeze owns is always the empty one an entry had just opened.

The alternative keeps the freeze and makes the side activation the incident
instead of the host: a fresh token at the boundary, the host left parked. It
keeps `correlate`'s contract, and needs the incident to record its host's
token — a column, for a failure whose cause can be told directly.

### D12 — what repair does not cover

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

### D13 — non-goals

- **No repair button.** The inspector is read-only, and a repair API is
  exactly the pressure the standing rule anticipates; the element pane is
  where it will arrive. Diagnosing which dispositions are legal is a read
  (D10). Offering the button is not.
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

`Lease.tla`'s `NeverStranded` comment records the cost: the engine does have
stranded items — a sibling branch's open task on an instance this item froze
— and a one-item model cannot express it. `spec/LeaseSiblings.tla` does:
`Lease` instantiated twice on one instance, sharing its status and the
database clock, so the transcription checked is the one the engine runs.

- `LeaseSiblings_Stranded.cfg` fails, and is meant to. It is `NeverStranded`
  asked of both items, and its trace is the motivating case: one item fails
  past its budget, and the other — open, perhaps leased mid-handler — can be
  neither claimed nor completed. The price of D3, checked instead of argued.
- `StrandedOnlyByASiblingsFreeze` holds: a stranded item is always its
  sibling's freeze, never its own doing.
- `FreezeAdvancesNothing` holds: on a frozen instance nothing is claimed,
  completed, failed or cancelled, and the one change an item can still take
  is its holder handing it back. Composing it is what showed `Lease`'s `Fail`
  and `FailFinally` lacked the `active` conjunct the fail path has
  (`IncidentOpen`) — invisible with one item, whose only freeze closes it.
- `ActiveStrandsNobody` holds: a final failure a boundary catches (`Lease`'s
  `FailCaught`) closes its item and freezes nothing, so no sibling is
  stranded by it. `LeaseSiblings_CaughtIsReachable.cfg` shows that case is
  reached rather than assumed.

What repair adds is a transition out of `active = FALSE`, which no model has
today because the freeze is terminal. `spec/Repair.tla` models it: one
instance with a sibling item leased across the freeze, and operators whose
requests name an incident and may arrive twice. A repair lands or freezes the
instance again under a new number; an abandon terminates it. The module
checks that a repair or an abandon lands only on the incident it named — a
resent or stale request is answered typed, never stepped: `BoundaryExit`'s
`LateCallsAreTyped` and `Lease`'s `ReleaseFreesOnlyTheLeaseItNamed` in one
shape — and that a landed repair leaves the sibling claimable or completable
again. A config without the number check is its counterexample. D8's re-arm
races nothing: the scheduler never picks a frozen instance's timer, and the
move commits with the repair. `LockOrder` needs nothing at either arity: a
repair is a step — the instance row, then its rows.

## The guarantees this preserves

1. An incident is **total**: after it, nothing on the instance is claimed,
   completed, failed or cancelled until an operator acts. The one change its
   items can still take is a holder extending or handing back its lease
   (`FreezeAdvancesNothing`, `spec/LeaseSiblings.tla`).
2. **Frozen evidence**: what is inspected is the state at the moment of
   failure — leases aside, the one thing (1) says still moves — and it is
   the state a repair resumes into.
3. **Token conservation**: nothing is lost, so a repair can be total and has
   exactly one shape to resume from.
4. **One gate**: `i.status = 'active'`, in five places, each provably
   complete.
5. No instance is ever half-done without saying so.
6. **A repair is exact**: it lands only on the incident it names (D9), it
   resumes every token the freeze held (D7), and the instance's clock stood
   still while it was frozen (D8).

D1 spends none of them, and neither does repair: it resumes into (2) and
depends on (3), which D11 makes whole. The rejected alternatives spend (2)
and (3).

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
and the failed work item it is parked at — are never muted; they are the
reason, not an arm. A failed item with no incident token at its element was
caught, and is drawn as handled.

**With one (D1).** The catch-all on `notify` takes the side path to an end of
its own, `REPLY` correlates, and the instance completes; `notify` shows as a
failure the model handled, not as an incident.

## Slices

**Slice 1 — the catch-all (D1 + D2): shipped.** Fixtures first, both directions: an
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

**Slice 2 — the failing `Lease` config: shipped** as `spec/LeaseSiblings.tla`.

**Slice 3 — repair in the core.** Scenarios first, red by design: a
`repair` action in the scenario runner, and golden traces for each
disposition at each row of finding 2's table; the refusals (Advance at a
gateway, Divert into nothing, Abandon on a main-flow branch, a stale
incident); collateral resuming — an in-flight sibling completing a join, a
pending decision asked again; a repair that re-freezes; and D11's refused
delivery. Then the core: `WaitKind::Halted`, the incident counter,
`Command::Repair`, `incident-repaired` (a new stable `Display` line),
side-path membership in compile, D11's check in `DeliverMessage`. The
explorer takes every repair as a stimulus on every failed state and asserts
D6 over the whole space. No lint change: repair is not a model capability.

**Slice 4 — through the engine.** The migration (two wait kinds, the incident
counter, `frozen_at`), persistence, `Engine::repair`, D8 in `persist_step`,
the typed errors and D11's refusal on `correlate`, the HTTP route, and
`commands_from` rebuilding the command so replay covers it. Engine tests: a
sibling's lease completing after a repair, the timer move, a resent repair.
Storm and chaos repair some of what they freeze. The README's upgrade note
gains the new wait kinds, which an older engine cannot load.

**Slice 5 — `spec/Repair.tla`**, its counterexample config, `spec/README`,
and `LockOrder` re-read.

**Slice 6 — what the operator sees.** The inspection carries the open
incident and what each disposition would do (D10); the inspector shows it as
text, draws halted tokens as frozen collateral, and a repaired failure as
repaired rather than "handled by a boundary". `bpmn-engine-design.md`'s
incident-freeze paragraph and `docs/dmn.md`'s pointer follow.

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
- **Repair**: slices 3–5 — scenarios per disposition and resume shape, the
  explorer over every repaired state, engine and HTTP tests, repaired
  instances replayed after storm and chaos, and `spec/Repair.tla`.

## Known warts, stated up front

- **The catch-all reopens one DMN ruling.** `docs/dmn.md` refused catchable
  decision failures partly because a boundary needs a code and DMN has none.
  A catch-all needs none, so that leg is gone; the ruling stands on its other
  one — a failed decision raises no error — and a catch-all does not reach it.
  Whether it should is open, and deliberately not answered by this slice.
- **D2 warns on nearly every side path.** Ten of the eleven side-path
  fixtures that predate it carry it, because a side path exists to run a task
  and the task is what fails. None of them is a false positive. It ships as
  it is deliberately: it is the lesson the motivating case teaches, and the
  fix is one boundary.
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
- **A Retry runs the handler again.** At-least-once, as ever: a repair does
  not change what a handler must tolerate.
- **A sibling's handler may run twice.** A worker whose completion was
  refused during the freeze keeps its lease (`worker.rs`); if the lease
  lapses before the repair, the item is claimed again after it.
- **Advance at a host whose boundary could not arm completes work that never
  ran.** The operator is saying so, and the event records why.
- **An instance frozen before the migration with several `incident` tokens
  can only be abandoned** (D7).
- **D11 moves one failure to the sender.** When some other branch has left
  a boundary's key unusable, every delivery to it is refused until the
  document changes — loud at the sender, and invisible to anyone reading the
  instance.
