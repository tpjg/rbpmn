---------------------------- MODULE DeleteInstance ----------------------------
(***************************************************************************)
(* `delete_instance`: the operator's escape hatch, and the only way a       *)
(* `failed` instance ever goes away.                                        *)
(*                                                                          *)
(* It has the sweep's shape — probe, archive, delete — and the same open    *)
(* gap: the sink reaches an object store, so no transaction is held across *)
(* it (`Retention.tla` says why). What the sweep does not share is its      *)
(* premise. The sweep selects `completed`/`terminated` records, which are   *)
(* immutable across the gap. `delete_instance` also accepts `failed`, and   *)
(* `failed` is not terminal: a repair thaws it (`Repair.tla`), steps it,    *)
(* and may freeze it again, all while the record is at the sink.            *)
(*                                                                          *)
(* The instance is abstracted to its status and its event count — every    *)
(* transition writes at least one event, and nothing but a deletion         *)
(* removes one. The deletion runs in one transaction under the instance     *)
(* row lock, so it is one atomic step here; the probe and the archive's     *)
(* load are two separate reads with the gap between and after them.         *)
(*                                                                          *)
(* Recheck selects what the deletion re-checks under the lock:              *)
(*   "full"    status still deletable AND the event count the probe read    *)
(*             (shipped)                                                    *)
(*   "status"  status still deletable — misses a thaw-and-refreeze          *)
(*   "none"    the original code: the probe's verdict is trusted            *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS
    MaxEvents,  \* history bound, to keep the model finite
    Recheck     \* "full" | "status" | "none"

ASSUME Recheck \in {"full", "status", "none"}

Deletable == {"failed", "completed"}

VARIABLES
    status,     \* "active" | "failed" | "completed" | "gone"
    events,     \* the instance's event count
    phase,      \* the deleter: "idle" | "probed" | "archived"
    seen,       \* the event count the probe read with the status
    archivedN,  \* how many events the archived copy carries
    liveGone,   \* history: an instance was deleted while active
    lostEvents  \* history: an event was deleted that no archive carries

vars == <<status, events, phase, seen, archivedN, liveGone, lostEvents>>

TypeOK ==
    /\ status \in {"active", "failed", "completed", "gone"}
    /\ events \in 1..MaxEvents
    /\ phase \in {"idle", "probed", "archived"}
    /\ seen \in 0..MaxEvents
    /\ archivedN \in 0..MaxEvents
    /\ liveGone \in BOOLEAN
    /\ lostEvents \in BOOLEAN

\* The instance begins frozen on an incident: the case that reaches here.
Init ==
    /\ status = "failed"
    /\ events = 1
    /\ phase = "idle"
    /\ seen = 0
    /\ archivedN = 0
    /\ liveGone = FALSE
    /\ lostEvents = FALSE

---------------------------------------------------------------------------
\* The process side. Each is a step under the instance lock and writes events.

\* A repair lands (`incident-repaired`): the instance is active again.
Repair ==
    /\ status = "failed"
    /\ events < MaxEvents
    /\ status' = "active"
    /\ events' = events + 1
    /\ UNCHANGED <<phase, seen, archivedN, liveGone, lostEvents>>

\* An active instance steps; it may freeze again or complete.
Step ==
    /\ status = "active"
    /\ events < MaxEvents
    /\ status' \in {"active", "failed", "completed"}
    /\ events' = events + 1
    /\ UNCHANGED <<phase, seen, archivedN, liveGone, lostEvents>>

---------------------------------------------------------------------------
\* The deleter.

\* The probe: status and event count in one statement, no lock held after.
Probe ==
    /\ phase = "idle"
    /\ status \in Deletable
    /\ phase' = "probed"
    /\ seen' = events
    /\ UNCHANGED <<status, events, archivedN, liveGone, lostEvents>>

\* `load_records` then the sink: a later read than the probe, still no lock.
\* A sink failure deletes nothing and is not interesting here.
Archive ==
    /\ phase = "probed"
    /\ phase' = "archived"
    /\ archivedN' = events
    /\ UNCHANGED <<status, events, seen, liveGone, lostEvents>>

\* One transaction: lock the row, re-check, delete. A refusal returns the
\* deleter to idle — the operator may call again.
Delete ==
    /\ phase = "archived"
    /\ status /= "gone"
    /\ phase' = "idle"
    /\ LET lands == CASE Recheck = "full"   -> status \in Deletable /\ events = seen
                      [] Recheck = "status" -> status \in Deletable
                      [] Recheck = "none"   -> TRUE
       IN IF lands
          THEN /\ status' = "gone"
               /\ liveGone' = (liveGone \/ status = "active")
               /\ lostEvents' = (lostEvents \/ events > archivedN)
               /\ UNCHANGED <<events, seen, archivedN>>
          ELSE UNCHANGED <<status, events, seen, archivedN, liveGone, lostEvents>>

Next == Repair \/ Step \/ Probe \/ Archive \/ Delete

Spec == Init /\ [][Next]_vars

---------------------------------------------------------------------------

(***************************************************************************)
(* "Refuses an active instance": not only at the probe, but at the          *)
(* deletion, whatever happened in the gap.                                  *)
(***************************************************************************)
NoLiveInstanceDeleted == ~liveGone

(***************************************************************************)
(* "No archive, no deletion", per event rather than per instance: what the  *)
(* sink accepted is the whole record that was deleted.                      *)
(***************************************************************************)
NoEventDeletedUnarchived == ~lostEvents

(***************************************************************************)
(* The model is not vacuous: the refused deleter can come back and delete. *)
(***************************************************************************)
NeverDeleted == status /= "gone"

=============================================================================
