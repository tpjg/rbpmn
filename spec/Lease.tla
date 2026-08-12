-------------------------------- MODULE Lease --------------------------------
(***************************************************************************)
(* The work-item lease (design brief, "Task API"): a short renewable TTL,   *)
(* renewed by holders while they work, expiring back to available with no   *)
(* reaper — the availability predicate is                                   *)
(* `state='available' OR lock_until < now()`.                               *)
(*                                                                          *)
(* The subtlety worth modelling is that a worker cannot observe its own     *)
(* expiry. After a lease lapses and a peer reacquires, BOTH workers believe *)
(* they hold the item. Safety therefore cannot come from preventing that    *)
(* state — it is reachable, and `DoubleBeliefIsReachable` exists to prove   *)
(* it. It comes from every mutation being conditional on                    *)
(* `lock_owner = $me AND lock_until > now()` in the database, so belief is  *)
(* never authority.                                                         *)
(*                                                                          *)
(* Claims checked here:                                                     *)
(*   - a late heartbeat after reacquisition loses (typed LockLost)          *)
(*   - completion authority follows the current lease, not the belief       *)
(*   - at-least-once delivery, exactly-once state transition                *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS
    Workers,   \* competing consumers of one topic
    NoOne,     \* the unheld marker
    TTL,       \* lease duration
    MaxTime    \* clock bound, to keep the model finite

ASSUME NoOne \notin Workers
ASSUME TTL \in Nat /\ TTL > 0
ASSUME MaxTime \in Nat

VARIABLES
    state,        \* "available" | "locked" | "done"
    owner,        \* Workers \cup {NoOne} — the lock_owner column
    until,        \* lock_until
    now,          \* database time
    believes,     \* Workers -> BOOLEAN: this worker thinks it holds the item
    completions   \* how many times the item transitioned to done

vars == <<state, owner, until, now, believes, completions>>

Time == 0..MaxTime
Deadline == 0..(MaxTime + TTL)

\* Exactly the SQL predicate: owner matches AND the lease has not lapsed.
HoldsLive(w) == state = "locked" /\ owner = w /\ until > now

\* Exactly `CLAIMABLE`: available, or locked with a lapsed lease.
Claimable == state = "available" \/ (state = "locked" /\ until <= now)

TypeOK ==
    /\ state \in {"available", "locked", "done"}
    /\ owner \in Workers \cup {NoOne}
    /\ until \in Deadline
    /\ now \in Time
    /\ believes \in [Workers -> BOOLEAN]
    /\ completions \in 0..2

Init ==
    /\ state = "available"
    /\ owner = NoOne
    /\ until = 0
    /\ now = 0
    /\ believes = [w \in Workers |-> FALSE]
    /\ completions = 0

\* Database time advances. Nothing tells a holder its lease lapsed.
Tick ==
    /\ now < MaxTime
    /\ now' = now + 1
    /\ UNCHANGED <<state, owner, until, believes, completions>>

\* get_task: one statement, SKIP LOCKED, expired leases count as available.
Acquire(w) ==
    /\ Claimable
    /\ state' = "locked"
    /\ owner' = w
    /\ until' = now + TTL
    \* The displaced holder is NOT notified — its belief persists. That is
    \* the whole point.
    /\ believes' = [believes EXCEPT ![w] = TRUE]
    /\ UNCHANGED <<now, completions>>

\* extend_lock: conditional UPDATE, owner and liveness both required.
Extend(w) ==
    /\ HoldsLive(w)
    /\ until' = now + TTL
    /\ UNCHANGED <<state, owner, now, believes, completions>>

\* ...and the typed LockLost when the condition fails: the client learns it
\* was reassigned rather than failing silently.
ExtendLost(w) ==
    /\ believes[w]
    /\ ~HoldsLive(w)
    /\ believes' = [believes EXCEPT ![w] = FALSE]
    /\ UNCHANGED <<state, owner, until, now, completions>>

\* complete_task: owner-checked under the instance lock.
Complete(w) ==
    /\ HoldsLive(w)
    /\ state' = "done"
    /\ completions' = completions + 1
    /\ believes' = [believes EXCEPT ![w] = FALSE]
    /\ UNCHANGED <<owner, until, now>>

\* A worker that lost the lease tries to complete and is refused.
CompleteRefused(w) ==
    /\ believes[w]
    /\ state = "locked"
    /\ ~HoldsLive(w)
    /\ believes' = [believes EXCEPT ![w] = FALSE]
    /\ UNCHANGED <<state, owner, until, now, completions>>

\* At-least-once delivery: a retried completion of a closed item is the
\* idempotent no-op, NOT a second state transition.
CompleteAlreadyClosed(w) ==
    /\ state = "done"
    /\ believes' = [believes EXCEPT ![w] = FALSE]
    /\ UNCHANGED <<state, owner, until, now, completions>>

Next ==
    \/ Tick
    \/ \E w \in Workers :
        Acquire(w) \/ Extend(w) \/ ExtendLost(w)
            \/ Complete(w) \/ CompleteRefused(w) \/ CompleteAlreadyClosed(w)

Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------

(***************************************************************************)
(* No double delivery: at most one worker can act on the item at a time.    *)
(* Note this is about *authority*, not belief — see below.                  *)
(***************************************************************************)
AtMostOneLiveHolder ==
    \A v, w \in Workers : (HoldsLive(v) /\ HoldsLive(w)) => v = w

(***************************************************************************)
(* Exactly-once state transition, under at-least-once delivery: however     *)
(* many times workers retry, the item completes once.                       *)
(***************************************************************************)
CompletedAtMostOnce == completions <= 1

(***************************************************************************)
(* Completion authority follows the current lease. A completed item was     *)
(* completed by whoever held it — never by a worker that merely believed.   *)
(***************************************************************************)
CompletionFollowsTheLease ==
    (state = "done") => (owner \in Workers /\ completions = 1)

(***************************************************************************)
(* An expired lease always returns the item to the queue: no reaper, and    *)
(* no item is ever stranded in a state nobody may claim or finish.          *)
(***************************************************************************)
NeverStranded ==
    (state = "locked" /\ ~(\E w \in Workers : HoldsLive(w))) => Claimable

(***************************************************************************)
(* Deliberately FALSE. Checked by Lease_DoubleBelief.cfg, which expects a   *)
(* violation: two workers really can both believe they hold the item, and   *)
(* the protocol is safe anyway. If this ever became true the model would be *)
(* asserting something weaker than reality.                                 *)
(***************************************************************************)
DoubleBeliefIsReachable ==
    ~ (\E v, w \in Workers : v # w /\ believes[v] /\ believes[w])

=============================================================================
