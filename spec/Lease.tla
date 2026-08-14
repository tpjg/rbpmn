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
(***************************************************************************)
EXTENDS Naturals

CONSTANTS
    Workers,   \* competing consumers of one topic
    NoOne,     \* the unheld marker
    TTL,       \* lease duration
    Backoff,   \* retry delay after a failure
    Retries,   \* failures before the instance freezes as an incident
    MaxTime    \* clock bound, to keep the model finite

ASSUME NoOne \notin Workers
ASSUME TTL \in Nat /\ TTL > 0
ASSUME MaxTime \in Nat

VARIABLES
    state,        \* "available" | "locked" | "done"
    owner,        \* the lock_owner column
    until,        \* lock_until
    retryAt,      \* retry_at: set on failure, blocks claiming until due
    retries,      \* remaining retry budget
    active,       \* the owning instance's status is 'active'
    now,          \* database time
    believes,     \* Workers -> BOOLEAN: this worker thinks it holds the item
    completions   \* how many times the item transitioned to done

vars ==
    <<state, owner, until, retryAt, retries, active, now, believes, completions>>

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

\* Database time advances. Nothing tells a holder its lease lapsed.
Tick ==
    /\ now < MaxTime
    /\ now' = now + 1
    /\ UNCHANGED <<state, owner, until, retryAt, retries, active, believes, completions>>

\* get_task / the worker claim: one statement, SKIP LOCKED.
Acquire(w) ==
    /\ Claimable
    /\ state' = "locked"
    /\ owner' = w
    /\ until' = now + TTL
    \* The displaced holder is NOT notified — its belief persists.
    /\ believes' = [believes EXCEPT ![w] = TRUE]
    /\ UNCHANGED <<retryAt, retries, active, now, completions>>

\* extend_lock: owner and liveness both required.
Extend(w) ==
    /\ HoldsLive(w)
    /\ until' = now + TTL
    /\ UNCHANGED <<state, owner, retryAt, retries, active, now, believes, completions>>

\* ...and the typed LockLost when that condition fails.
ExtendLost(w) ==
    /\ believes[w]
    /\ ~HoldsLive(w)
    /\ believes' = [believes EXCEPT ![w] = FALSE]
    /\ UNCHANGED <<state, owner, until, retryAt, retries, active, now, completions>>

Complete(w) ==
    /\ Completable(w)
    /\ state' = "done"
    /\ completions' = completions + 1
    /\ believes' = [believes EXCEPT ![w] = FALSE]
    /\ UNCHANGED <<owner, until, retryAt, retries, active, now>>

\* Refused by the guard: another worker's lease is live.
CompleteRefused(w) ==
    /\ state # "done"
    /\ ~GuardAllows(w)
    /\ believes' = [believes EXCEPT ![w] = FALSE]
    /\ UNCHANGED <<state, owner, until, retryAt, retries, active, now, completions>>

\* At-least-once delivery: retrying a closed item is the idempotent no-op,
\* NOT a second state transition. This is what keeps `completions` at one —
\* the property is earned here rather than being structurally impossible.
CompleteAlreadyClosed(w) ==
    /\ state = "done"
    /\ believes' = [believes EXCEPT ![w] = FALSE]
    /\ UNCHANGED <<state, owner, until, retryAt, retries, active, now, completions>>

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
    /\ UNCHANGED <<active, now, completions>>

FailFinally(w) ==
    /\ state = "locked"
    /\ GuardAllows(w)
    /\ retries = 0
    /\ active' = FALSE          \* incident: the instance freezes for repair
    /\ believes' = [believes EXCEPT ![w] = FALSE]
    /\ UNCHANGED <<state, owner, until, retryAt, retries, now, completions>>

Next ==
    \/ Tick
    \/ \E w \in Workers :
        \/ Acquire(w) \/ Extend(w) \/ ExtendLost(w)
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
(* Deliberately FALSE — checked by Lease_DoubleBelief.cfg, which expects a  *)
(* violation. Two workers really can both believe they hold the item, and   *)
(* the protocol is safe anyway.                                            *)
(***************************************************************************)
DoubleBeliefIsReachable ==
    ~ (\E v, w \in Workers : v # w /\ believes[v] /\ believes[w])

=============================================================================
