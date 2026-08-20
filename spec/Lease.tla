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
(* Its first property, `NoForeignHandBack`, was deleted for the same reason *)
(* as the idealised `Complete` above, from the opposite direction: it said  *)
(* `(HoldsLive(w) /\ v # w) => ~ReleaseGuard(v)`, which is true by          *)
(* propositional reasoning in every state, reachable or not, because        *)
(* `HoldsLive(w)` already carries `owner = w`. It restated the guard's text *)
(* instead of constraining the protocol's behaviour, so it would have kept  *)
(* holding had `Release` been given a different guard, or a body that       *)
(* freed a live holder by some other route. What replaced it is about the   *)
(* *effect* and needs `lastActor` to say it — see                           *)
(* `LiveLeaseEndsOnlyByItsHolder`.                                          *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS
    Workers,   \* competing consumers of one topic
    NoOne,     \* the unheld marker
    NoLease,   \* "this step named no epoch" — a model value, like NoOne
    TTL,       \* lease duration
    Backoff,   \* retry delay after a failure
    Retries,   \* failures before the instance freezes as an incident
    MaxTime,   \* clock bound, to keep the model finite
    MaxLeases, \* epoch bound, likewise: claim/release can cycle in one instant
    UncheckedRelease, \* TRUE = drop release_task's owner check (a bug)
    EpochlessRelease  \* TRUE = drop its lease_no check (the shipped bug)

ASSUME NoOne \notin Workers
ASSUME NoLease \notin 0..MaxLeases
ASSUME TTL \in Nat /\ TTL > 0
ASSUME MaxTime \in Nat
ASSUME MaxLeases \in Nat /\ MaxLeases > 0
ASSUME UncheckedRelease \in BOOLEAN
ASSUME EpochlessRelease \in BOOLEAN

VARIABLES
    state,        \* "available" | "locked" | "done"
    owner,        \* the lock_owner column
    until,        \* lock_until
    retryAt,      \* retry_at: set on failure, blocks claiming until due
    retries,      \* remaining retry budget
    active,       \* the owning instance's status is 'active'
    now,          \* database time
    believes,     \* Workers -> BOOLEAN: this worker thinks it holds the item
    completions,  \* how many times the item transitioned to done
    lastActor,    \* who took the step: a worker, or NoOne for the clock
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

\* `complete_work_item_in_tx` refuses on a frozen instance (IncidentOpen).
Completable(w) == state # "done" /\ active /\ GuardAllows(w)

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
    /\ state \in {"available", "locked", "done"}
    /\ owner \in Workers \cup {NoOne}
    /\ until \in Deadline
    /\ retryAt \in Deadline
    /\ retries \in 0..Retries
    /\ active \in BOOLEAN
    /\ now \in Time
    /\ believes \in [Workers -> BOOLEAN]
    /\ completions \in 0..2
    /\ lastActor \in Workers \cup {NoOne}
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
CompleteAlreadyClosed(w) ==
    /\ state = "done"
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

FailFinally(w) ==
    /\ state = "locked"
    /\ GuardAllows(w)
    /\ retries = 0
    /\ active' = FALSE          \* incident: the instance freezes for repair
    /\ believes' = [believes EXCEPT ![w] = FALSE]
    /\ lastActor' = w
    /\ named' = NoLease
    /\ UNCHANGED <<state, owner, until, retryAt, retries, now, completions, leaseNo, issued>>

Next ==
    \/ Tick
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
(* An item can be neither claimable nor completable — but only for a stated *)
(* reason: it is inside its retry backoff, or its instance froze as an      *)
(* incident. The earlier `NeverStranded` asserted this could not happen at  *)
(* all, which was only true because the model had no failure path.          *)
(***************************************************************************)
Blocked ==
    /\ state # "done"
    /\ ~Claimable
    /\ ~(\E w \in Workers : Completable(w))

StrandedOnlyForAStatedReason ==
    Blocked => (retryAt > now \/ ~active)

(***************************************************************************)
(* What the epoch buys, and it takes a behavioural property to say it: a    *)
(* release that *lands* named the lease that was actually current. The      *)
(* engine cannot distinguish a retry from a fresh request by looking at the *)
(* caller — same task, same owner, same statement — so the only way to      *)
(* state the difference is to record what the request named and compare it  *)
(* with what was there when it arrived.                                     *)
(*                                                                          *)
(* Note that `LiveLeaseEndsOnlyByItsHolder` cannot catch this, and that is  *)
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
(***************************************************************************)
LiveLeaseEndsOnlyByItsHolder ==
    [][ \A w \in Workers :
          (HoldsLive(w) /\ ~HoldsLive(w)' /\ now' = now) => lastActor' = w ]_vars

(***************************************************************************)
(* Deliberately FALSE — checked by Lease_DoubleBelief.cfg, which expects a  *)
(* violation. Two workers really can both believe they hold the item, and   *)
(* the protocol is safe anyway.                                            *)
(***************************************************************************)
DoubleBeliefIsReachable ==
    ~ (\E v, w \in Workers : v # w /\ believes[v] /\ believes[w])

=============================================================================
