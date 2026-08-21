# TLA+ specs — the concurrency protocol

`docs/stress-testing.md` §7, item 10. These model the **protocol**, not the
code: Rust-level tools cannot reach these claims, because the behaviour lives
in PostgreSQL's concurrency semantics rather than in the Rust.

Run with `just tla` (needs `java`; fetches `tla2tools.jar` on first use).

| Spec | Models | Checks |
|---|---|---|
| `LockOrder.tla` | **every lock-taking transaction shape in the engine** — step, timer claim, work claim, retention, deploy — over per-instance rows plus the definition and floor rows | nobody holds rows while still needing the instance row; no AB/BA deadlock; every transaction returns to idle |
| `Lease.tla` | the work-item lease: TTL, renewal, expiry, completion, the voluntary hand-back, the **process withdrawing the item** (interrupting boundary, terminate, teardown), and clients retrying their own requests | no double delivery; exactly-once completion under at-least-once delivery; a live lease ends only by the clock, its own holder, or the process; a cancelled item is never completed; a release frees only the lease it named; never stranded — an open item is always claimable or completable |
| `TimerTeardown.tla` | the unlocked pick of an **arm row** — a timer by the scheduler, a boundary subscription by `correlate` — racing a scope teardown, and a claim transaction that rolls back after its re-check | no armed row — timer or subscription — outlives the token it is armed on; no arm ever fires with its token gone |
| `BoundaryExit.tla` | one token at a host work item with an interrupting boundary subscription; `complete_task` and `correlate` racing to end the wait, from any node | exactly one exit ever reaches `step`; an armed row always means an open host; a late call of either verb is answered typed (`AlreadyClosed`, `NoSubscription`), never stepped |
| `Retention.tla` | a retention pass across its transaction-free archive gap | nothing deleted without an archive; the truncation floor covers every deletion and invents none; only due records go |

Each spec ships with a companion config that is **expected to fail**, so the
checks are known to have teeth rather than passing vacuously:

| Config | Expected | Demonstrates |
|---|---|---|
| `LockOrder.cfg` | holds | the shipped protocol |
| `LockOrderHistorical.cfg` | **deadlock** | the timer-claim order the design brief rejected |
| `Lease.cfg` | holds | the shipped lease |
| `Lease_DoubleBelief.cfg` | **violation** | two workers really can both believe they hold one item |
| `Lease_UncheckedRelease.cfg` | **violation** | `release_task` without its owner check, freeing a live holder's item |
| `Lease_EpochlessRelease.cfg` | **violation** | `release_task` without its lease epoch — a retried release freeing the claim that replaced it |
| `Lease_CancelIgnoresGuard.cfg` | **violation** | completion without its `AlreadyClosed` check — a clerk's decision landing on a task the process had withdrawn |
| `TimerTeardown.cfg` | holds | the shipped teardown |
| `TimerTeardown_Buggy.cfg` | **violation** | the phase-6 bug: teardown reaping tokens but not their timers |
| `SubscriptionTeardown.cfg` | holds | the same module with `Timers` bound to subscription rows — `correlate`'s claim against teardown |
| `BoundaryExit.cfg` | holds | the shipped pair: completion withdraws the arm, delivery re-checks its own row |
| `BoundaryExit_NoRecheck.cfg` | **violation** | `correlate` stepping on its unlocked pick: a completion in the window, then a second exit |
| `BoundaryExit_NoWithdraw.cfg` | **violation** | completion leaving the boundary's subscription row behind — an arm outliving its wait |
| `BoundaryExit_AnyRowRecheck.cfg` | **violation** | a re-check satisfied by *some* open subscription instead of *this* one — a late delivery reaching `step` where the contract says 404 |
| `Retention.cfg` | holds | the shipped pass |
| `Retention_FloorFromPlan.cfg` | **violation** | advancing the floor from the plan instead of the deletions |
| `Retention_NoRecheck.cfg` | **violation** | trusting the plan's DUE verdict across the archive gap |

## What DMN changed here, and what it did not

The claim path used to run a pure `step` under the instance lock. It now runs
`step_answering_decisions`, which reads the definition's DMN from the database
and evaluates it **inside the same transaction**. Two things follow, and both
are written into the model rather than argued in a commit message — the
standing warning in CLAUDE.md is that a hand-written model does not adapt and
nothing fails when it goes stale.

* A claim can now abort *after* its re-check passed, which was not previously
  reachable from this path. `TimerTeardown.tla`'s `Abort` action models it. It
  leaves state identical to `Drop`, because a rollback returns the claim
  instead of consuming it — the identity is the point, and TLC checking it is
  what makes it more than a claim.
* No new lock enters the order. The decision cache is an in-process
  `RwLock<HashMap>` of the same shape as the compiled-process cache that was
  already taken here, never held across an `await`; the
  `rbpmn_definition_decision` read takes no row locks and so cannot join a
  cycle. `LockOrder.tla` models database locks and needs no new arity.

## What message boundaries changed here

The design round for message boundary events
(`docs/design/boundary-messages.md`, §8) touched three models and added one.
Per the standing warning above, each was **re-read**, not just re-run.

**`LockOrder` — re-read, unchanged.** `correlate_in_tx` keeps its shape:
resolve the subscription without a lock, `FOR UPDATE` on the instance row,
re-check, step, persist. A boundary subscription is a row in the same table
found by the same index. Cancelling a *leased* work item is
`set_work_item_state(..., 'cancelled')` inside that same transaction — a
write to a per-instance row already covered by the instance lock; a lease is
a row value, not a lock, so the holder's open lease blocks nothing and
nothing new can be waited for. `guard_lease` and `tasks.rs` were re-read for
the holder's side: every verb either reads the item row under the instance
lock (`complete`, `fail`) or is a single conditional `UPDATE` (`extend`,
`release`), and none takes a lock the inventory table does not already list.
No new lock enters the order at either arity.

**`Lease.tla` — the actor the model never had.** The only exits from a claim
were the holder's verbs and the clock. But terminate and the interrupting
timer boundary have cancelled leased items since phase 3, so the shipped
engine never had `LiveLeaseEndsOnlyByItsHolder`; it held because nothing in
the model could do what the engine does. A message boundary makes that actor
a human-triggered one (a payment arriving while the task is open), which made
the omission worth closing. `Cancel` is transcribed from `persist_step`'s
`WorkItemCancelled` handling — the state column changes and nothing else; no
owner check, no liveness clause, nobody told. The property is now
`LiveLeaseEndsOnlyByItsHolderOrTheProcess`, and what the lease still
guarantees once the process has acted is `NoCompletionAfterCancel`: the
holder's later `complete_task` lands as `AlreadyClosed { state: "cancelled" }`,
its `extend` and `release` as `Lost`. `Lease_CancelIgnoresGuard.cfg` drops
the closed-item check and TLC produces the trace in two steps: cancel, then
complete. Transcribing `Cancel` also corrected `FailFinally`: the model used
to leave a finally-failed item `locked` on the frozen instance, which the
engine never does — `persist_step` writes `failed`, the same
state-column-only write. That removed the only state
`StrandedOnlyForAStatedReason` ever had to say something about (checked: with
`~Blocked` as an invariant TLC finds no violation — an `available` item in
its retry backoff is completable, because `guard_lease` never reads
`retry_at`), so the property is `NeverStranded` again, with its antecedent
reachable; see the file for the one-item caveat. (It is an action property rather than an invariant for the reason
the file gives: a completion that ignored the check would move the row to
`done`, and an invariant over `state = "cancelled"` would never see the
state it guards.) A cancelled item is the second terminal state the lease
configs run with `-deadlock` for, beside the frozen-instance one.

**`TimerTeardown.tla` — a second instance of the same protocol.** Nothing in
the module is timer-specific: it models an arm row picked without a lock,
a teardown that may commit in the window, and a re-check that confirms the
row and not the token. `correlate` claims a boundary subscription exactly
that way, and teardown withdraws subscriptions and timers in the same loop
(`withdraw_arms(Some(token))` beside `tokens.remove`).
`SubscriptionTeardown.cfg` binds `Timers` to subscription rows and holds
with identical state counts; the prose now says "arm row" where it said
"timer".

**`BoundaryExit.tla` — the property the design asked for.** Activity
completion and boundary triggering are mutually exclusive on one activation.
The engine earns that from two things, and each has a config that removes it:
completion withdraws the boundary's row in its own transaction
(`ArmDiesWithTheWait`; `BoundaryExit_NoWithdraw.cfg` keeps the row and TLC
stops one step in, at an armed row on a completed host), and delivery
re-checks *its* row under the instance lock (`BoundaryExit_NoRecheck.cfg`
steps on the unlocked pick: Pick, Complete, Deliver — two exits;
`BoundaryExit_AnyRowRecheck.cfg` re-checks "some open subscription" and a
sibling branch's catch lets a late delivery through to `step`, which
`LateCallsAreTyped` catches). One thing the model deliberately leaves out,
stated in its header: the core's own `UnknownSubscription` would turn the
NoRecheck trace into an internal error rather than a second exit. Crediting
it would prove safety through a check the design does not intend to rely on;
the point of the re-check is the *typed* 404, and `stepped` is what records
that the re-check, not the core, stood between a late message and a closed
task. Invariants are checked in config order, so each failing config lists
first the invariant its `just tla` row names.

Found while writing it, and worth recording beside the Retention note below:
`stepped' = stepped \/ ~armed \/ …` is `(stepped' = stepped) \/ …` — the
variable went unconstrained exactly when it should have become TRUE, and the
first run of `BoundaryExit_AnyRowRecheck` reported the wrong invariant. The
parentheses are in the file with a comment pointing here.

### Slice 2 (non-interrupting boundaries): re-read, nothing to model

A non-interrupting boundary spawns a *sibling* token and leaves the host
untouched. Re-read against `step.rs::spawn_side_token` (pure: a new token id,
`element-started`/`element-completed` on the boundary, `leave_single`) and
`persist_step` (tokens are written as a snapshot under the instance row lock,
like every token): no new lock enters the order at either arity, so
`LockOrder` is unchanged. `BoundaryExit` is unaffected for a different
reason: a non-interrupting boundary never competes with the host's
completion — the item stays open, both verbs succeed, and the only exit race
is still the interrupting one. The message re-arm is a new arm row on the
same token, written in the delivering transaction (there is never a
committed state with a live host and no arm), and teardown withdraws it with
the token like any other row — `SubscriptionTeardown.cfg` already covers
that shape.

### Slice 3 (`timeCycle`): re-read, nothing to model

A cycle's re-arm is a new `rbpmn_timer` row inserted in the firing
transaction, after the fired row's delete — both under the instance row
lock the claim already holds, so the claim path's shape (`try_fire`: pick,
NOWAIT, re-check, step, persist) and `LockOrder` are unchanged. For
`TimerTeardown` the re-armed row is an arm row on the same token, withdrawn
with it like any other; the scheduler's re-check still sees the row and
never the token, and the invariant that protects it is the same one.

## What the failures show

`LockOrderHistorical` restores the originally sketched timer claim — timer row
first, then the instance row — which is the opposite order from every other
step path. TLC reports `Deadlock reached`.

Checking the model also separated two things the prose around this tends to
conflate. Deleting `GiveUp` — making the timer claim block like every other
path — does **not** reintroduce the deadlock: with a single lock order there
is no cycle to find, whoever waits. **Deadlock freedom comes from the order,
not from `NOWAIT`.** `NOWAIT` buys something else, and `scheduler.rs` says
what: an embedder's `*_in_tx` transaction can hold an instance row for a long
time, and the drain loop must move on to other instances' timers rather than
park behind it. Order is the safety property; `NOWAIT` is throughput.

`TimerTeardown_Buggy` reproduces a bug that actually shipped and was fixed in
the phase-6 review. When an error was caught by a boundary on an *enclosing*
subprocess, the failing task's token was removed before teardown ran, so
teardown — which only reaps tokens still present — never withdrew that token's
armed boundary timers. The timer row outlived its token; the scheduler later
picked it, re-checked it under the instance lock, found the row still there,
fired it, and wedged the instance on an `Invariant`. TLC finds it in five
states: `Arm -> Pick -> Teardown -> Fire`.

Modelling it separates the defence from the guarantee. The scheduler's
re-check under the instance lock verifies the timer **row** still exists — it
never verifies the row's **token** does. Those come apart exactly when
teardown is incomplete, so the safety of the whole claim path rests on an
invariant of *teardown* (`ArmedTimersHaveLiveTokens`), not on the re-check.
The re-check is necessary and insufficient, and only the model says so out
loud.

`Lease_DoubleBelief` shows that after a lease lapses and a peer reacquires,
both workers believe they hold the item. That state is *reachable and fine*:
every mutation is conditional on `lock_owner = $me AND lock_until > now()` in
the database, so belief is never authority. Safety does not come from
preventing the confusion — it comes from the confusion not mattering.

## The lock inventory `LockOrder` covers

Traced from the code on the third audit, because the first two both missed
paths. Every shape that takes a lock:

| Path | Order | First lock waits? |
|---|---|---|
| step (`runtime.rs`) | instance row → its per-instance rows | blocking |
| timer claim (`scheduler.rs`) | [try-advisory] → instance row → rows | NOWAIT |
| work claim (`tasks.rs`, `worker.rs`) | one work-item row, **no instance row** | SKIP LOCKED |
| retention (`retention.rs`) | instance rows → definition row → floor row | SKIP LOCKED |
| `delete_definition` | definition row → policy row | blocking |
| deploy (`deploy.rs`) | advisory(key) → definition rows | blocking |
| declared index build (`tasks.rs`) | [try-advisory(instance indexes)] → **no row locks at all** | try only |

`retire` and `deploy` were outside the model until the third audit. Three
things are deliberately *not* modelled and are argued instead: the
scheduler's `pg_try_advisory_xact_lock`, the migration advisory, and the
declared index build's slot. All are excluded for the same reason — a
try-lock never waits, so it cannot be an edge in a wait-for cycle; the
migration one also runs only at startup.

The index-build slot earns a paragraph, because it is the one place where
"use a blocking lock, it is simpler" is not merely worse but **wrong**, and
the wrongness was measured rather than reasoned:

- `CREATE INDEX CONCURRENTLY` waits for every concurrent snapshot to drain
  before it finishes. A session blocked on a lock is a session holding a
  snapshot. So a blocking lock around the build closes a cycle: the holder
  waits for the waiter's snapshot, the waiter waits for the holder's lock.
  Postgres reported this as a genuine deadlock, on the first run of
  `concurrent_deploys_of_one_shared_field`.
- Removing the lock does not help either: two `CREATE INDEX CONCURRENTLY` on
  one *table* deadlock the same way, one waiting for
  ShareUpdateExclusive while the other waits for its snapshot. That hazard
  **predates** scoped indexes — two definitions deploying at once already
  both index `rbpmn_instance` — and is reproduced by
  `concurrent_deploys_of_different_indexes_do_not_deadlock`, which fails
  against the old code.
- Hence: a try-lock, polled, with the session **idle between attempts**. The
  idleness is load-bearing, not tidiness — it is what lets the holder's build
  drain. And the key is the *table*, not the index name, because two
  different indexes on one table deadlock exactly as two builds of one index
  do.

It takes no row lock and no transaction, and it cannot overlap deploy's
advisory: `apply_manifest_indexes` runs strictly after the deploy transaction
commits, because a CONCURRENTLY build cannot run inside one.

Inverting retention's order in the model (floor → definition → instance)
violates the invariant, so these kinds are not decoration.

## What the Lease spec does and does not claim

`Complete` is transcribed from `guard_lease`, which refuses only when
**another** owner holds a **live** lease. It does not require the completer to
hold anything — it cannot, because push-mode workers and the ownerless HTTP
path complete items too. So the checked property is
`NoLiveForeignCompletion`: a worker demonstrably still working cannot have its
item completed underneath it. An earlier version of this spec required a live
lease to complete and therefore proved a protocol the engine does not
implement, which is worse than having no spec: it manufactures confidence.

`NeverStranded` was deleted once as holding only because the model had no
failure path, replaced by `StrandedOnlyForAStatedReason`, and is back: with
the failure path written as the engine writes it, a one-item model has no
state in which an open item is neither claimable nor completable (the
backoff gates claiming, not the ownerless completion), and a property with
an unreachable antecedent proves nothing. The stranded items the engine does
have — a sibling branch's open task on a frozen instance — need a second
item to model. See "What message boundaries changed here".

`Release` — `release_task`, the voluntary hand-back — is the third exit from
a claim, and the only one that returns an item to the queue with nothing
having gone wrong. Its guard is transcribed like the others:
`lock_owner = $me AND state = 'locked'`, with **no** liveness clause, because
an expired lease nobody reclaimed still names its owner and releasing it only
tidies the row.

`LiveLeaseEndsOnlyByItsHolder` is the property that check earns: a live lease
ends by the clock or by its own holder's hand, never by anyone else's. It has
to be an **action** property, because the thing being ruled out is a
transition rather than a state — no predicate over one state can distinguish
"this item became available" from "*someone else* made this item available".
That is what the `lastActor` variable is for.

The first attempt was `NoForeignHandBack ==
(HoldsLive(w) /\ v # w) => ~ReleaseGuard(v)`, and it was deleted rather than
kept alongside: `HoldsLive(w)` already carries `owner = w`, so it is true by
propositional reasoning in every state, reachable or not. It restated the
guard's text instead of constraining behaviour, and would have gone on
holding if `Release` had been given a different guard or a body that freed a
live holder some other way — the same failure as the idealised `Complete`
above, approached from the opposite side. The replacement quantifies over the
whole of `Next`, so it checks the four guards *together*: `CLAIMABLE` keeps
`Acquire` off a live lease, `guard_lease` keeps `Complete` and `Fail` off it,
`ReleaseGuard` keeps `Release` off it. `believes` could not have expressed
any of this — two workers believing at once is already reachable and already
safe.

## The lease epoch, and the request the model could not see

`LiveLeaseEndsOnlyByItsHolder` is about *who* acted, and there is a bug it
cannot see, because in that bug the holder and the actor are the same worker.

`release_task` first shipped guarded by owner and state alone. The whole task
API assumes at-least-once delivery of client requests, and every other verb
survives it — `extend_lock` carries `lock_until > now()`, a repeated
completion converges on `AlreadyClosed`. Release did not: a client whose
release committed but whose response was lost retries it, and because the
released item is available again and *oldest*, it is the very item FIFO hands
back to that same client on its next claim. The retry then matches — same
task, same owner, same statement — and frees a live claim somebody is looking
at.

`just tla` was green over this the whole time, and not by accident: **the
model had no notion of a request**. A replay and a fresh release were the
same step, so no property could separate them. The action that was missing is
`ReleaseReplay`, and it needs the model to record which release requests were
actually sent (`issued`), so that a retry re-offers one of those and nothing
else. An earlier draft let a replay name any epoch below the current one, and
TLC promptly produced a counterexample replaying epoch 0 — an epoch no client
can ever have been given, since epochs come from claims and a claim always
yields at least 1. A counterexample nobody can reach is worse than none: the
config is the documentation of the bug, so it has to show the bug that
happens.

The property is `ReleaseFreesOnlyTheLeaseItNamed`: a release that *lands*
named the lease that was actually current. It needs `named` for the same
reason the other one needs `lastActor` — the difference between a retry and a
fresh request is not visible in any single state, only in what the step
carried. `Lease_EpochlessRelease.cfg` checks **both** properties, and only
this one fires; that is the demonstration that they are not redundant.

Modelling it also made terminal states reachable, which is why the lease
configs run with `-deadlock`: a closed item — completed, cancelled, or failed
with its instance frozen for a repair this model does not include — once the
clock has run out. Those are legitimate end states, the same kind as a
torn-down scope; deadlock freedom remains a property under test only for
`LockOrder`. (The first version of this note described a release into a
frozen instance, a state reachable only while `FailFinally` left the item
`locked`.)

## Retention and the archive gap

`Retention.tla` models the one place phase 7 is subtle: `plan` → archive →
`execute` deliberately holds **no transaction** across the sink call (an open
transaction would pin the cluster-wide `pg_snapshot_xmin` the event stream's
horizon is built on). Everything in the model follows from that gap being
open — `skip locked` dropping planned instances, a policy changing under the
sweep, two sweepers overlapping.

The two floor obligations pull against each other, which is what makes them
worth checking together: nothing deleted may sit *above* the floor (or a
reader above it loses events silently), and the floor may not sit above
everything actually deleted (or readers get `CursorTruncated` for nothing).
The second is exactly what `delete_records`' comment warns about, and
`Retention_FloorFromPlan.cfg` is that mistake made on purpose.

TLC also found a bug in this spec while checking it: `undue' = undue \/ X`
parses as `(undue' = undue) \/ X`, so the variable went unassigned whenever
`X` was true. The shipped config never noticed — `X` is unreachable there —
and only the counterexample config exposed it.

## Scope

`LockOrder` is parameterised over the per-instance rows a step touches
(`RowOrder`), and is checked at two arities because they answer different
questions:

- `LockOrder.cfg` / `LockOrderHistorical.cfg` — 3 nodes, 2 instances, **2
  rows**: the cross-instance question, where contention between nodes lives.
- `LockOrderAllRows.cfg` / `...Historical.cfg` — 2 nodes, 1 instance, **all
  five rows** a step really touches since phase 6 (tokens, work items,
  timers, subscriptions, scopes): the arity question.

The all-rows run finishes in 112 states, and that number is the point rather
than a disappointment: once a node holds the instance row, no peer can take
any of that instance's rows, so per-instance rows are **never contended**.
Arity adds path length, not branching. That is *why* the ordering rule scales
to however many tables a step grows — but it is now checked at the real
number instead of asserted from two.

Bounded models: 3 nodes / 2 instances / 2 row kinds, 2 workers / TTL 2 / 4
ticks, 2 nodes / 2 tokens / 2 timers (or subscriptions), and 2 nodes / 1
token / 1 boundary subscription plus a sibling row. That
is enough for these properties — the interleavings that break lock ordering
and lease authority need two participants, not many — but it is exhaustive
only within those bounds, not a proof for all N.

The specs are hand-maintained abstractions. They are only as true as their
correspondence to `scheduler.rs`, `runtime.rs` (`correlate_in_tx`,
`persist_step`) and `tasks.rs`; nothing enforces that link. Changing the locking or lease protocol means changing
these too.

Not modelled yet: the event-stream safe horizon (xid8 visibility) and the
deploy/undeclare race. Both are named in `docs/stress-testing.md` as
candidates.

A caution the phase-6 round earned: **these specs do not adapt themselves.**
Phase 6 landed and `spec/` was not touched — the conclusion that scope rows
change nothing here was an argument, later verified by checking that all four
`rbpmn_scope` access sites sit under the instance row lock, but an argument
nonetheless. When the locking or lease protocol changes, these files must be
re-read; nothing will fail to tell you.
