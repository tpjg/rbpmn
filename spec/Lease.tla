-------------------------------- MODULE Lease --------------------------------
(***************************************************************************)
(* The work-item lease, modelled on the predicates the code actually runs.  *)
(*                                                                          *)
(* An earlier version of this spec proved something stronger than the       *)
(* engine implements — `Complete` required the completer to hold a live     *)
(* lease, while `guard_lease` only refuses when *another* owner holds a     *)
(* *live* one. A model that proves more than the code does is worse than no *)
(* model: it manufactures confidence. So the guard here is transcribed      *)
(* rather than idealised, and the property is whatever survives that.       *)
(*                                                                          *)
(* What survives is narrower and true: a worker demonstrably still working  *)
(* on an item cannot have it completed out from under it. Completion is NOT *)
(* gated on holding a lease — it cannot be, because push-mode workers and   *)
(* the ownerless HTTP path complete items too. The lease protects a live    *)
(* holder; it does not confer exclusive authority on anyone else's behalf.  *)
(*                                                                          *)
(* Two further corrections, both from review:                               *)
(*   - `Claimable` now carries all four conjuncts of `CLAIMABLE`, including *)
(*     the retry backoff and the instance being active. Without them the    *)
(*     model could not express the states where an item really is stuck.    *)
(*   - exactly-once is *earned* rather than structural: `Complete` is       *)
(*     enabled in several states, so the closed-item no-op is what keeps    *)
(*     the count at one.                                                    *)
(*                                                                          *)
(* `Release` (the voluntary hand-back) is a lease transition like any       *)
(* other and is modelled as one: it is the third way a claim ends, and the  *)
(* first that returns an item to the queue with nothing having gone wrong.  *)
(*                                                                          *)
(* `Cancel` is the fourth, and the one this model lacked longest: the       *)
(* *process* withdrawing the item — an interrupting boundary, a terminate,  *)
(* a scope teardown. Terminate and the interrupting timer boundary have     *)
(* cancelled leased items since phase 3, so the shipped engine never had    *)
(* the property this file used to state, `LiveLeaseEndsOnlyByItsHolder`: a  *)
(* live lease has always been ended by the process as well. The model had   *)
(* no action for it, so the property held — vacuously over the one actor it *)
(* left out. Message boundaries (docs/design/boundary-messages.md §8) made  *)
(* that actor a human-triggered one and the omission worth closing. The     *)
(* property is now `LiveLeaseEndsOnlyByItsHolderOrTheProcess`, which is the *)
(* honest one, and what the lease actually buys is stated by what Cancel    *)
(* must NOT do: the holder's later verbs all land as typed answers          *)
(* (`AlreadyClosed`, `Lost`), never as a completion (`NoCompletionAfterCancel`). *)
(*                                                                          *)
(* Its first property, `NoForeignHandBack`, was deleted for the same reason *)
(* as the idealised `Complete` above, from the opposite direction: it said  *)
(* `(HoldsLive(w) /\ v # w) => ~ReleaseGuard(v)`, which is true by          *)
(* propositional reasoning in every state, reachable or not, because        *)
(* `HoldsLive(w)` already carries `owner = w`. It restated the guard's text *)
(* instead of constraining the protocol's behaviour, so it would have kept  *)
(* holding had `Release` been given a different guard, or a body that       *)
(* freed a live holder by some other route. What replaced it is about the   *)
(* *effect* and needs `lastActor` to say it — see                           *)
(* `LiveLeaseEndsOnlyByItsHolderOrTheProcess`.                              *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS
    Workers,   \* competing consumers of one topic
    NoOne,     \* the unheld marker
    Process,   \* the actor when the engine itself withdraws the item
    NoLease,   \* "this step named no epoch" — a model value, like NoOne
    TTL,       \* lease duration
    Backoff,   \* retry delay after a failure
    Retries,   \* failures before the instance freezes as an incident
    MaxTime,   \* clock bound, to keep the model finite
    MaxLeases, \* epoch bound, likewise: claim/release can cycle in one instant
    UncheckedRelease, \* TRUE = drop release_task's owner check (a bug)
    EpochlessRelease, \* TRUE = drop its lease_no check (the shipped bug)
    CompleteIgnoresClosed \* TRUE = drop completion's AlreadyClosed check (a bug)

ASSUME NoOne \notin Workers
ASSUME Process \notin Workers /\ Process # NoOne
ASSUME CompleteIgnoresClosed \in BOOLEAN
ASSUME NoLease \notin 0..MaxLeases
ASSUME TTL \in Nat /\ TTL > 0
ASSUME MaxTime \in Nat
ASSUME MaxLeases \in Nat /\ MaxLeases > 0
ASSUME UncheckedRelease \in BOOLEAN
ASSUME EpochlessRelease \in BOOLEAN

VARIABLES
    state,        \* "available" | "locked" | "done" | "cancelled" | "failed"
    owner,        \* the lock_owner column
    until,        \* lock_until
    retryAt,      \* retry_at: set on failure, blocks claiming until due
    retries,      \* remaining retry budget
    active,       \* the owning instance's status is 'active'
    now,          \* database time
    believes,     \* Workers -> BOOLEAN: this worker thinks it holds the item
    completions,  \* how many times the item transitioned to done
    lastActor,    \* who took the step: a worker, NoOne for the clock, Process for the engine
    leaseNo,      \* rbpmn_work_item.lease_no: bumped by every claim
    named,        \* the epoch the step's release named; NoLease otherwise
    issued        \* Workers -> the release requests it has actually sent

vars ==
    <<state, owner, until, retryAt, retries, active, now, believes, completions,
      lastActor, leaseNo, named, issued>>

\* Epochs are bounded only to keep the model finite: Acquire and Release can
\* cycle any number of times inside one instant, so an unbounded lease_no
\* would be an infinite state space. Two is already enough to express the
\* bug (claim, release, re-claim, replay).
Leases == 0..MaxLeases

Time == 0..MaxTime
Deadline == 0..(MaxTime + TTL + Backoff)

\* `lock_owner = $me AND lock_until > now()` — the condition every mutating
\* statement in tasks.rs carries.
HoldsLive(w) == state = "locked" /\ owner = w /\ until > now

\* Exactly `CLAIMABLE` (crates/rbpmn-engine/src/lib.rs): free or lapsed, past
\* any retry backoff, and the instance still active.
\*
\* `until < now`, not `<= now`: the SQL is `lock_until < now()` for claiming
\* and `lock_until > now()` for holding, so at the single instant
\* `lock_until = now()` an item is *neither* claimable nor live. Writing `<=`
\* here would make the two predicates partition the timeline and quietly
\* close a gap the database leaves open.
Claimable ==
    /\ state = "available" \/ (state = "locked" /\ until < now)
    /\ retryAt <= now
    /\ active

\* Exactly `guard_lease` (runtime.rs): refuse only when the item is locked,
\* the lease is live, and the caller is not its owner. Note what is NOT
\* required — that the caller holds anything at all.
GuardAllows(w) == ~(state = "locked" /\ until > now /\ owner # w)

\* `complete_work_item_in_tx`: the AlreadyClosed no-op comes first — only an
\* `available` or `locked` item is completed; `completed`, `cancelled` and
\* `failed` all answer AlreadyClosed before the core is invoked — then the
\* frozen-instance refusal (IncidentOpen), then guard_lease. `state # "done"`
\* was enough while the only closed state was `done`; `Cancel` makes the
\* transcription matter, and `CompleteIgnoresClosed` drops it to show why.
Completable(w) ==
    /\ IF CompleteIgnoresClosed THEN state # "done"
                               ELSE state \in {"available", "locked"}
    /\ active
    /\ GuardAllows(w)

\* Exactly `release_task` (tasks.rs): `lock_owner = $me AND lease_no = $mine
\* AND state = 'locked'`, for a request naming epoch `e`.
\*
\* Note what is deliberately absent — liveness. An expired lease nobody
\* reclaimed still names its owner, and releasing it only tidies the row; the
\* clock is not what the guard is for.
\*
\* Note what is deliberately present. The owner is redundant for identifying
\* the claim — the epoch does that alone — and is kept because epochs are
\* small integers, so without it anyone holding a task id could end a
\* stranger's claim. Each half has its own counterexample config.
ReleaseGuard(w, e) ==
    /\ state = "locked"
    /\ UncheckedRelease \/ owner = w
    /\ EpochlessRelease \/ e = leaseNo

TypeOK ==
    /\ state \in {"available", "locked", "done", "cancelled", "failed"}
    /\ owner \in Workers \cup {NoOne}
    /\ until \in Deadline
    /\ retryAt \in Deadline
    /\ retries \in 0..Retries
    /\ active \in BOOLEAN
    /\ now \in Time
    /\ believes \in [Workers -> BOOLEAN]
    /\ completions \in 0..2
    /\ lastActor \in Workers \cup {NoOne, Process}
    /\ leaseNo \in Leases
    /\ named \in Leases \cup {NoLease}
    /\ issued \in [Workers -> SUBSET Leases]

Init ==
    /\ state = "available"
    /\ owner = NoOne
    /\ until = 0
    /\ retryAt = 0
    /\ retries = Retries
    /\ active = TRUE
    /\ now = 0
    /\ believes = [w \in Workers |-> FALSE]
    /\ completions = 0
    /\ lastActor = NoOne
    /\ leaseNo = 0
    /\ named = NoLease
    /\ issued = [w \in Workers |-> {}]

\* Database time advances. Nothing tells a holder its lease lapsed.
Tick ==
    /\ now < MaxTime
    /\ now' = now + 1
    /\ lastActor' = NoOne
    /\ named' = NoLease
    /\ UNCHANGED <<state, owner, until, retryAt, retries, active, believes,
                   completions, leaseNo, issued>>

\* get_task / the worker claim: one statement, SKIP LOCKED.
Acquire(w) ==
    /\ Claimable
    /\ leaseNo < MaxLeases          \* model bound, not a protocol rule
    /\ state' = "locked"
    /\ owner' = w
    /\ until' = now + TTL
    \* A claim, and only a claim, mints an epoch. `Extend` renews a lease
    \* without bumping it: the epoch changes exactly when the right to act
    \* changes hands, which is what makes a stale request identifiable.
    /\ leaseNo' = leaseNo + 1
    \* The displaced holder is NOT notified — its belief persists.
    /\ believes' = [believes EXCEPT ![w] = TRUE]
    /\ lastActor' = w
    /\ named' = NoLease
    /\ UNCHANGED <<retryAt, retries, active, now, completions, issued>>

\* extend_lock: owner and liveness both required.
Extend(w) ==
    /\ HoldsLive(w)
    /\ until' = now + TTL
    /\ lastActor' = w
    /\ named' = NoLease
    /\ UNCHANGED <<state, owner, retryAt, retries, active, now, believes,
                   completions, leaseNo, issued>>

\* ...and the typed LockLost when that condition fails.
ExtendLost(w) ==
    /\ believes[w]
    /\ ~HoldsLive(w)
    /\ believes' = [believes EXCEPT ![w] = FALSE]
    /\ lastActor' = w
    /\ named' = NoLease
    /\ UNCHANGED <<state, owner, until, retryAt, retries, active, now,
                   completions, leaseNo, issued>>

\* release_task: the third exit from a claim. Back to available with no
\* backoff and no budget spent — unlike Fail, nothing went wrong, so the item
\* is on offer again at once rather than after a delay or a lease.
\*
\* Parameterized by the epoch the *request* carries, which is the whole point:
\* `e = leaseNo` is a client releasing the claim it is holding, and `e <
\* leaseNo` is a request issued against an earlier claim arriving late. The
\* engine cannot tell those apart by looking at the caller, and neither can
\* this model — only the epoch separates them.
ReleaseWith(w, e) ==
    /\ ReleaseGuard(w, e)
    /\ state' = "available"
    /\ owner' = NoOne
    /\ until' = 0
    /\ believes' = [believes EXCEPT ![w] = FALSE]
    /\ lastActor' = w
    /\ named' = e
    \* The request is now on the wire, and may arrive again.
    /\ issued' = [issued EXCEPT ![w] = @ \cup {e}]
    /\ UNCHANGED <<retryAt, retries, active, now, completions, leaseNo>>

\* At-least-once delivery of the client's own requests: a release whose
\* response never came back arrives a second time. It re-offers exactly the
\* requests this worker really sent — an earlier draft let it replay any
\* epoch below the current one, and TLC dutifully produced a counterexample
\* replaying epoch 0, which no client can ever have been given (an epoch
\* comes from a claim, and a claim always yields at least 1). A
\* counterexample nobody can reach is worse than none: it is the config's
\* documentation, and it must show the bug that actually happens.
\*
\* This is the action the model was missing when `release_task` first
\* shipped. Without it a replay and a fresh release are the same step, and
\* `just tla` stays green over the whole hazard.
ReleaseReplay(w) == \E e \in issued[w] : ReleaseWith(w, e)

\* ...and the typed Released::Lost when the statement matches no row. A
\* replay that names a spent epoch lands here, which is the fix working:
\* the client is told its claim is gone rather than silently freeing one.
ReleaseLost(w) ==
    /\ believes[w]
    /\ \A e \in Leases : ~ReleaseGuard(w, e)
    /\ believes' = [believes EXCEPT ![w] = FALSE]
    /\ lastActor' = w
    /\ named' = NoLease
    /\ UNCHANGED <<state, owner, until, retryAt, retries, active, now,
                   completions, leaseNo, issued>>

Complete(w) ==
    /\ Completable(w)
    /\ state' = "done"
    /\ completions' = completions + 1
    /\ believes' = [believes EXCEPT ![w] = FALSE]
    /\ lastActor' = w
    /\ named' = NoLease
    /\ UNCHANGED <<owner, until, retryAt, retries, active, now, leaseNo, issued>>

\* Refused by the guard: another worker's lease is live.
CompleteRefused(w) ==
    /\ state # "done"
    /\ ~GuardAllows(w)
    /\ believes' = [believes EXCEPT ![w] = FALSE]
    /\ lastActor' = w
    /\ named' = NoLease
    /\ UNCHANGED <<state, owner, until, retryAt, retries, active, now,
                   completions, leaseNo, issued>>

\* At-least-once delivery: retrying a closed item is the idempotent no-op,
\* NOT a second state transition. This is what keeps `completions` at one —
\* the property is earned here rather than being structurally impossible.
\* A cancelled item answers the same way: `AlreadyClosed { state: "cancelled" }`
\* is what a lease holder gets after the process withdrew its task.
CompleteAlreadyClosed(w) ==
    /\ state \in {"done", "cancelled", "failed"}
    /\ believes' = [believes EXCEPT ![w] = FALSE]
    /\ lastActor' = w
    /\ named' = NoLease
    /\ UNCHANGED <<state, owner, until, retryAt, retries, active, now,
                   completions, leaseNo, issued>>

\* fail_work_item_in_tx: back to available behind a backoff, budget spent.
\* Exhausting it raises an incident, which freezes the instance.
Fail(w) ==
    /\ state = "locked"
    /\ GuardAllows(w)
    /\ retries > 0
    /\ state' = "available"
    /\ owner' = NoOne
    /\ until' = 0
    /\ retryAt' = now + Backoff
    /\ retries' = retries - 1
    /\ believes' = [believes EXCEPT ![w] = FALSE]
    /\ lastActor' = w
    /\ named' = NoLease
    /\ UNCHANGED <<active, now, completions, leaseNo, issued>>

\* RaiseError: the core emits WorkItemFailed and persist_step writes
\* `state = 'failed'` — the state column and nothing else, like Cancel. This
\* model used to leave the item `locked` here, which the engine never does;
\* a finally-failed item answers AlreadyClosed { state: "failed" } exactly as
\* a cancelled one does, and is closed, not stranded.
FailFinally(w) ==
    /\ state = "locked"
    /\ GuardAllows(w)
    /\ retries = 0
    /\ state' = "failed"
    /\ active' = FALSE          \* incident: the instance freezes for repair
    /\ believes' = [believes EXCEPT ![w] = FALSE]
    /\ lastActor' = w
    /\ named' = NoLease
    /\ UNCHANGED <<owner, until, retryAt, retries, now, completions, leaseNo, issued>>

\* The process withdraws the item: an interrupting boundary on the host, a
\* terminate end, the teardown of an enclosing scope. Transcribed from
\* `persist_step`'s handling of `WorkItemCancelled` — `set_work_item_state(...,
\* 'cancelled')` — which changes the state column and nothing else: the lease
\* columns keep whatever they held, and nobody is told (`believes` stands).
\* There is deliberately NO liveness clause and NO owner check: a lease is a
\* row value that protects a holder from *other workers*, never from the
\* process. It needs an active instance, because every path that cancels is
\* a step, and a frozen instance takes no steps.
\*
\* A cancelled item is terminal: no action below moves it (the guards of
\* Acquire, Extend, Release, Complete and Fail all exclude it), which is the
\* second legitimate end state these configs run with `-deadlock` for.
Cancel ==
    /\ state \in {"available", "locked"}
    /\ active
    /\ state' = "cancelled"
    /\ lastActor' = Process
    /\ named' = NoLease
    /\ UNCHANGED <<owner, until, retryAt, retries, active, now, believes,
                   completions, leaseNo, issued>>

Next ==
    \/ Tick
    \/ Cancel
    \/ \E w \in Workers :
        \/ Acquire(w) \/ Extend(w) \/ ExtendLost(w)
        \/ ReleaseWith(w, leaseNo) \/ ReleaseReplay(w) \/ ReleaseLost(w)
        \/ Complete(w) \/ CompleteRefused(w) \/ CompleteAlreadyClosed(w)
        \/ Fail(w) \/ FailFinally(w)

Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------

(***************************************************************************)
(* At most one worker can act on the item at a time — about *authority*,    *)
(* not belief. Two workers can both believe (see DoubleBeliefIsReachable).  *)
(***************************************************************************)
AtMostOneLiveHolder ==
    \A v, w \in Workers : (HoldsLive(v) /\ HoldsLive(w)) => v = w

(***************************************************************************)
(* Exactly-once state transition under at-least-once delivery. Now a real   *)
(* obligation: `Complete` is enabled whenever the guard allows, so without  *)
(* the closed-item no-op this would reach two.                             *)
(***************************************************************************)
CompletedAtMostOnce == completions <= 1

(***************************************************************************)
(* **The property the engine actually provides.** A live lease is never     *)
(* overridden: while a worker's lease is live, nobody else completes,       *)
(* fails, or extends its item. It is deliberately NOT "only a lease holder  *)
(* may complete" — push-mode workers and the ownerless HTTP path complete   *)
(* items without one, and the code says so.                                 *)
(***************************************************************************)
NoLiveForeignCompletion ==
    (state = "done" /\ owner \in Workers /\ until > now) => completions = 1

(***************************************************************************)
(* An OPEN item is always claimable or completable. This property has gone  *)
(* back and forth, and the history is the lesson: `NeverStranded` was first *)
(* deleted as holding only because the model had no failure path, and       *)
(* replaced by `StrandedOnlyForAStatedReason == Blocked => (retryAt > now   *)
(* \/ ~active)`. Transcribing FailFinally as the engine writes it (`failed`, *)
(* a closed item) removed the `~active` case, and checking the other one    *)
(* showed it had never been reachable either: an `available` item inside    *)
(* its retry backoff is *completable* — `guard_lease` never reads           *)
(* `retry_at`; only claiming does — so `Blocked` was satisfied by nothing   *)
(* but the `locked`-after-failure artefact. A property whose antecedent is  *)
(* unreachable proves nothing, so it is the positive statement again, now   *)
(* with its antecedent reachable from the initial state.                    *)
(*                                                                          *)
(* What it does not say: the engine does have stranded items — a SIBLING    *)
(* branch's open task on an instance this item froze — and a one-item model *)
(* cannot express that. It would need a second item and a property about    *)
(* `~active` on it; until then this is the one-item truth, not the whole.   *)
(***************************************************************************)
Open == state \in {"available", "locked"}

NeverStranded ==
    Open => (Claimable \/ \E w \in Workers : Completable(w))

(***************************************************************************)
(* What the epoch buys, and it takes a behavioural property to say it: a    *)
(* release that *lands* named the lease that was actually current. The      *)
(* engine cannot distinguish a retry from a fresh request by looking at the *)
(* caller — same task, same owner, same statement — so the only way to      *)
(* state the difference is to record what the request named and compare it  *)
(* with what was there when it arrived.                                     *)
(*                                                                          *)
(* `LiveLeaseEndsOnlyByItsHolderOrTheProcess` cannot catch this, and that is *)
(* the point of having both: in the replay the holder *is* the actor. Alice *)
(* frees Alice's own live claim with Alice's own stale request, so every    *)
(* property phrased in terms of *who* acted holds while the item is handed  *)
(* to somebody else. Lease_EpochlessRelease.cfg drops `e = leaseNo` and TLC *)
(* produces the trace: claim, release, re-claim, retry.                     *)
(***************************************************************************)
ReleaseFreesOnlyTheLeaseItNamed ==
    [][ (state = "locked" /\ state' = "available" /\ named' # NoLease)
          => named' = leaseNo ]_vars

(***************************************************************************)
(* A live lease ends by the clock or by its own holder's hand, never by     *)
(* anyone else's. An **action** property, and it has to be: the thing being *)
(* ruled out is a transition, not a state — no predicate over a single      *)
(* state can distinguish "this item became available" from "*someone else*  *)
(* made this item available", which is why `lastActor` exists.              *)
(*                                                                          *)
(* It is not a restatement of any one guard. It quantifies over the whole   *)
(* of `Next`, so it is the four guards checked *together*: `CLAIMABLE`      *)
(* keeps `Acquire` off a live lease, `guard_lease` keeps `Complete` and     *)
(* `Fail` off it, `ReleaseGuard` keeps `Release` off it. Weaken any one of  *)
(* them, or add a fifth action that frees an item, and this fails —         *)
(* whereas the guard-restating version it replaced would not have noticed.  *)
(*                                                                          *)
(* `now' = now` is the clock's exemption, and it is the whole reason the    *)
(* lease model works without a reaper: time alone takes a lease away, from  *)
(* a holder that is never told. Belief cannot state any of this — two       *)
(* workers believing at once is already reachable and already safe          *)
(* (DoubleBeliefIsReachable). Lease_UncheckedRelease.cfg drops the owner    *)
(* check from `Release` alone and TLC produces the trace.                   *)
(*                                                                          *)
(* `Process` is the second exemption, and it was missing until message      *)
(* boundaries: the process withdrawing a leased item (Cancel) ends a live   *)
(* lease by neither the clock nor its holder. Stated as "OnlyByItsHolder"   *)
(* this property was never true of the shipped engine — terminate and the   *)
(* interrupting timer boundary have done exactly that since phase 3 — and   *)
(* it held only because the model had no action for the process. Adding     *)
(* Cancel is what exposed it; the name now says what is actually promised.  *)
(***************************************************************************)
LiveLeaseEndsOnlyByItsHolderOrTheProcess ==
    [][ \A w \in Workers :
          (HoldsLive(w) /\ ~HoldsLive(w)' /\ now' = now)
              => lastActor' \in {w, Process} ]_vars

(***************************************************************************)
(* What the lease does NOT protect against, and what it still guarantees    *)
(* once the process has acted. A cancelled item is never completed: the     *)
(* holder's later `complete_task` lands as `AlreadyClosed`, its `extend`    *)
(* and `release` as `Lost`. The invariant states the terminal state; the    *)
(* action property is the one with teeth, because a completion that ignored *)
(* the closed-item check would move the row to "done" and the invariant     *)
(* would never see the state it guards. Lease_CancelIgnoresGuard.cfg drops  *)
(* the check and TLC produces the trace: cancel, then complete.             *)
(***************************************************************************)
CancelledIsNeverCompleted == state = "cancelled" => completions = 0

NoCompletionAfterCancel ==
    [][ state = "cancelled" => completions' = completions ]_vars

(***************************************************************************)
(* Deliberately FALSE — checked by Lease_DoubleBelief.cfg, which expects a  *)
(* violation. Two workers really can both believe they hold the item, and   *)
(* the protocol is safe anyway.                                            *)
(***************************************************************************)
DoubleBeliefIsReachable ==
    ~ (\E v, w \in Workers : v # w /\ believes[v] /\ believes[w])

=============================================================================
