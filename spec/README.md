# TLA+ specs — the concurrency protocol

`docs/stress-testing.md` §7, item 10. These model the **protocol**, not the
code: Rust-level tools cannot reach these claims, because the behaviour lives
in PostgreSQL's concurrency semantics rather than in the Rust.

Run with `just tla` (needs `java`; fetches `tla2tools.jar` on first use).

| Spec | Models | Checks |
|---|---|---|
| `LockOrder.tla` | the instance row plus every per-instance row a step touches (`RowOrder`: tokens, work items, timers, subscriptions, scopes), across N nodes | one lock order for the *stepping* paths; no AB/BA deadlock; every transaction returns to idle |
| `Lease.tla` | the work-item lease: TTL, renewal, expiry, completion | no double delivery; exactly-once completion under at-least-once delivery; nothing stranded |
| `TimerTeardown.tla` | the scheduler's unlocked timer pick racing a scope teardown | no timer row outlives the token it is armed on; no timer ever fires with its token gone |

Each spec ships with a companion config that is **expected to fail**, so the
checks are known to have teeth rather than passing vacuously:

| Config | Expected | Demonstrates |
|---|---|---|
| `LockOrder.cfg` | holds | the shipped protocol |
| `LockOrderHistorical.cfg` | **deadlock** | the timer-claim order the design brief rejected |
| `Lease.cfg` | holds | the shipped lease |
| `Lease_DoubleBelief.cfg` | **violation** | two workers really can both believe they hold one item |
| `TimerTeardown.cfg` | holds | the shipped teardown |
| `TimerTeardown_Buggy.cfg` | **violation** | the phase-6 bug: teardown reaping tokens but not their timers |

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
ticks, and 2 nodes / 2 tokens / 2 timers. That
is enough for these properties — the interleavings that break lock ordering
and lease authority need two participants, not many — but it is exhaustive
only within those bounds, not a proof for all N.

The specs are hand-maintained abstractions. They are only as true as their
correspondence to `scheduler.rs`, `runtime.rs` and `tasks.rs`; nothing
enforces that link. Changing the locking or lease protocol means changing
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
