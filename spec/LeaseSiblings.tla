--------------------------- MODULE LeaseSiblings ---------------------------
(***************************************************************************)
(* Two work items on one instance: `Lease`, twice, sharing the instance's  *)
(* status and the database clock (docs/design/incident-scope.md, D3).     *)
(*                                                                          *)
(* `Lease.tla` is about ONE item, and its `NeverStranded` is the one-item   *)
(* truth: an open item is always claimable or completable. The instance-   *)
(* wide freeze makes that false for every OTHER item on the instance — a    *)
(* sibling's open task, a sibling worker's live lease that can never be     *)
(* completed — and a one-item model cannot express it, because the only     *)
(* thing that freezes its instance is its own item failing, which closes it.*)
(* This module instantiates the very same transcription rather than         *)
(* restating it, so what is checked here is the lease the engine runs, not  *)
(* a second model of it.                                                    *)
(*                                                                          *)
(* Shared: `active` (one instance row) and `now` (one database). Everything *)
(* else is per item. Either item takes a step of its own protocol while the *)
(* other stands still: every step on an instance serializes on its row      *)
(* lock, so interleaving is the whole of the concurrency there is.          *)
(*                                                                          *)
(* The properties come in a pair, and the pair is the point:               *)
(*   - `StrandedOnlyByASiblingsFreeze` holds: when an item is stranded, the *)
(*     instance is frozen and the OTHER item is the one that failed. The    *)
(*     freeze is the only cause, and it is never the stranded item's own.  *)
(*   - `SiblingNeverStranded` fails (LeaseSiblings_Stranded.cfg). It is the *)
(*     one-item `NeverStranded` asked of both, and TLC's trace is the price *)
(*     of the instance-wide freeze: one item fails past its budget, and the *)
(*     other — open, perhaps leased to a worker whose handler is still      *)
(*     running — can be neither claimed nor completed until an operator     *)
(*     acts. Not a bug: the decision, checked instead of argued.            *)
(*                                                                          *)
(* And `FreezeAdvancesNothing` holds: on a frozen instance no item is       *)
(* claimed, completed, failed or cancelled. The one change an item can     *)
(* still take is its holder handing it back, because `release_task` and    *)
(* `extend_lock` are single statements that never read instance status —   *)
(* so that is exactly what the property allows, and no more.               *)
(*                                                                         *)
(* And `ActiveStrandsNobody` holds: a final failure a boundary catches     *)
(* (`Lease`'s FailCaught) closes its item and freezes nothing, so no       *)
(* sibling is stranded by it. LeaseSiblings_CaughtIsReachable.cfg shows    *)
(* that case is reached, not assumed.                                      *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS
    Workers, NoOne, Process, NoLease, TTL, Backoff, Retries, MaxTime,
    MaxLeases, UncheckedRelease, EpochlessRelease, CompleteIgnoresClosed

VARIABLES
    active, now,
    state1, owner1, until1, retryAt1, retries1, believes1, completions1,
    lastActor1, leaseNo1, named1, issued1,
    state2, owner2, until2, retryAt2, retries2, believes2, completions2,
    lastActor2, leaseNo2, named2, issued2

item1 == <<state1, owner1, until1, retryAt1, retries1, believes1,
           completions1, lastActor1, leaseNo1, named1, issued1>>
item2 == <<state2, owner2, until2, retryAt2, retries2, believes2,
           completions2, lastActor2, leaseNo2, named2, issued2>>
vars == <<active, now, item1, item2>>

I1 == INSTANCE Lease WITH
    state <- state1, owner <- owner1, until <- until1, retryAt <- retryAt1,
    retries <- retries1, active <- active, now <- now, believes <- believes1,
    completions <- completions1, lastActor <- lastActor1,
    leaseNo <- leaseNo1, named <- named1, issued <- issued1

I2 == INSTANCE Lease WITH
    state <- state2, owner <- owner2, until <- until2, retryAt <- retryAt2,
    retries <- retries2, active <- active, now <- now, believes <- believes2,
    completions <- completions2, lastActor <- lastActor2,
    leaseNo <- leaseNo2, named <- named2, issued <- issued2

Init == I1!Init /\ I2!Init

Next ==
    \/ I1!Next /\ UNCHANGED item2
    \/ I2!Next /\ UNCHANGED item1

Spec == Init /\ [][Next]_vars

TypeOK == I1!TypeOK /\ I2!TypeOK

\* Composition takes nothing away from either item.
EachItemSafe ==
    /\ I1!AtMostOneLiveHolder /\ I2!AtMostOneLiveHolder
    /\ I1!CompletedAtMostOnce /\ I2!CompletedAtMostOnce
    /\ I1!NoLiveForeignCompletion /\ I2!NoLiveForeignCompletion
    /\ I1!CancelledIsNeverCompleted /\ I2!CancelledIsNeverCompleted

StrandedOnlyByASiblingsFreeze ==
    /\ ~I1!NeverStranded => (~active /\ state2 = "failed")
    /\ ~I2!NeverStranded => (~active /\ state1 = "failed")

\* Deliberately FALSE — LeaseSiblings_Stranded.cfg expects the violation.
SiblingNeverStranded == I1!NeverStranded /\ I2!NeverStranded

\* While the instance is active nobody is stranded, whatever has failed and
\* been caught. `Lease`'s FailCaught closes an item without freezing — the
\* catch-all's whole point — and this is its sibling staying claimable or
\* completable through it. It follows from the lease's guards once `active`
\* holds; LeaseSiblings_CaughtIsReachable.cfg is what shows a caught failure
\* leaves the instance active.
ActiveStrandsNobody == active => SiblingNeverStranded

\* Deliberately FALSE — LeaseSiblings_CaughtIsReachable.cfg expects the
\* violation. It shows the case ActiveStrandsNobody speaks of is reached: an
\* item failed past its budget on an instance still active.
NoFailureWasCaught == ~((state1 = "failed" \/ state2 = "failed") /\ active)

FreezeAdvancesNothing ==
    [][ ~active =>
          /\ (state1' # state1 => state1 = "locked" /\ state1' = "available")
          /\ (state2' # state2 => state2 = "locked" /\ state2' = "available") ]_vars

=============================================================================
