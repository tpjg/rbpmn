------------------------------ MODULE Retention ------------------------------
(***************************************************************************)
(* Phase 7 retention: the bounded deletion of history, and the truncation   *)
(* floor that keeps the event stream honest about it.                       *)
(*                                                                          *)
(* A pass is `plan` -> archive -> `execute`, and **the gap between them      *)
(* holds no transaction** — the sink reaches an object store, and any open  *)
(* transaction would hold back the cluster-wide `pg_snapshot_xmin` the       *)
(* stream's safe horizon is built on. Everything interesting follows from   *)
(* that gap being open:                                                     *)
(*                                                                          *)
(*   - instances may be stepped, so `for update ... skip locked` silently   *)
(*     drops some of the planned set;                                       *)
(*   - the policy that made a record due may change under the sweep, so     *)
(*     `execute_retention` re-applies the whole DUE predicate, not just the *)
(*     status;                                                              *)
(*   - two sweepers' leases may overlap.                                    *)
(*                                                                          *)
(* The floor is what readers rely on. From `events.rs`: "everything deleted *)
(* is at or below the floor, so a cursor at or above it has provably lost   *)
(* nothing, however scattered the deleted set" — a resume below it fails    *)
(* with CursorTruncated rather than skipping the gap silently. Two          *)
(* obligations fall out, and they pull in opposite directions:              *)
(*                                                                          *)
(*   FloorCoversDeletions        nothing deleted may sit ABOVE the floor,   *)
(*                              or a reader above it loses events silently  *)
(*   FloorIsSomethingDeleted     the floor may not sit above everything     *)
(*                              actually deleted, or readers are truncated  *)
(*                              for nothing                                 *)
(*                                                                          *)
(* `FloorFromPlan = TRUE` computes the floor from the planned set instead   *)
(* of the deleted set, which is exactly the mistake `delete_records` warns  *)
(* against; `RecheckDue = FALSE` drops the re-application of DUE under the  *)
(* row lock. Both are counterexample configs.                               *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS
    Instances,      \* terminal records eligible for retirement
    Positions,      \* event (txid, id) pairs, as a linear order 1..N
    Sweepers,       \* competing retention passes; leases may overlap
    FloorFromPlan,  \* TRUE = advance the floor from the plan, not the deletions
    RecheckDue      \* FALSE = trust the plan's DUE verdict across the gap

VARIABLES
    evInst,       \* Positions -> the instance an event belongs to (immutable)
    due,          \* Instances -> BOOLEAN: the DUE predicate holds right now
    busy,         \* instances being stepped; `skip locked` drops these
    gone,         \* instances deleted
    deletedEv,    \* events deleted (by cascade from their instance)
    floor,        \* the monotonic truncation floor
    archived,     \* instances handed to the sink successfully
    phase,        \* Sweepers -> "idle" | "planned" | "archived"
    plan,         \* Sweepers -> the instance set this pass selected
    undue         \* TRUE once something was deleted while not due

vars ==
    <<evInst, due, busy, gone, deletedEv, floor, archived, phase, plan, undue>>

EventsOf(I) == {p \in Positions : evInst[p] \in I}
MaxOf(S) == IF S = {} THEN 0 ELSE CHOOSE p \in S : \A q \in S : q <= p

TypeOK ==
    /\ evInst \in [Positions -> Instances]
    /\ due \in [Instances -> BOOLEAN]
    /\ busy \subseteq Instances
    /\ gone \subseteq Instances
    /\ deletedEv \subseteq Positions
    /\ floor \in 0..MaxOf(Positions)
    /\ archived \subseteq Instances
    /\ phase \in [Sweepers -> {"idle", "planned", "archived"}]
    /\ plan \in [Sweepers -> SUBSET Instances]
    /\ undue \in BOOLEAN

Init ==
    /\ evInst \in [Positions -> Instances]
    /\ due \in [Instances -> BOOLEAN]
    /\ busy = {}
    /\ gone = {}
    /\ deletedEv = {}
    /\ floor = 0
    /\ archived = {}
    /\ phase = [s \in Sweepers |-> "idle"]
    /\ plan = [s \in Sweepers |-> {}]
    /\ undue = FALSE

\* An instance is stepped, or stops being stepped. Only the *timing* matters
\* here: a stepped instance is skipped by `for update ... skip locked`.
ToggleBusy(i) ==
    /\ busy' = IF i \in busy THEN busy \ {i} ELSE busy \cup {i}
    /\ UNCHANGED <<evInst, due, gone, deletedEv, floor, archived, phase, plan, undue>>

\* `set_retention_policy` during a sweep — including an operator noticing a
\* mis-set age and switching a key back to `forever()` while a pass is
\* blocked on an upload.
TogglePolicy(i) ==
    /\ due' = [due EXCEPT ![i] = ~due[i]]
    /\ UNCHANGED <<evInst, busy, gone, deletedEv, floor, archived, phase, plan, undue>>

\* Phase one: select due, not-yet-gone records. Reads only; no lock is held
\* into the gap.
Plan(s) ==
    /\ phase[s] = "idle"
    /\ \E candidates \in SUBSET {i \in Instances : due[i] /\ i \notin gone} :
        plan' = [plan EXCEPT ![s] = candidates]
    /\ phase' = [phase EXCEPT ![s] = "planned"]
    /\ UNCHANGED <<evInst, due, busy, gone, deletedEv, floor, archived, undue>>

\* The sink succeeded. Export is at-least-once: overlapping sweepers archive
\* the same record twice, which is an idempotent overwrite, never a deletion.
ArchiveOk(s) ==
    /\ phase[s] = "planned"
    /\ archived' = archived \cup plan[s]
    /\ phase' = [phase EXCEPT ![s] = "archived"]
    /\ UNCHANGED <<evInst, due, busy, gone, deletedEv, floor, plan, undue>>

\* The sink failed: nothing is deleted, on every path.
ArchiveFailed(s) ==
    /\ phase[s] = "planned"
    /\ phase' = [phase EXCEPT ![s] = "idle"]
    /\ plan' = [plan EXCEPT ![s] = {}]
    /\ UNCHANGED <<evInst, due, busy, gone, deletedEv, floor, archived, undue>>

\* Phase two, one short transaction: re-check under the row lock, delete,
\* advance the floor.
Execute(s) ==
    /\ phase[s] = "archived"
    /\ LET eligible == {i \in plan[s] :
                          /\ i \notin gone
                          /\ i \notin busy          \* skip locked
                          /\ (RecheckDue => due[i]) \* the DUE re-check
                       }
           removed == EventsOf(eligible)
           \* The mistake `delete_records` guards against: taking the high
           \* water mark from the plan rather than from what was deleted.
           basis == IF FloorFromPlan THEN EventsOf(plan[s]) ELSE removed
           high == MaxOf(basis)
       IN /\ gone' = gone \cup eligible
          /\ deletedEv' = deletedEv \cup removed
          \* `where (txid, id) < (new)`: monotonic, never lowered.
          /\ floor' = IF high > floor THEN high ELSE floor
          \* Parenthesised deliberately: `=` binds tighter than `\/`, so
          \* `undue' = undue \/ X` is a disjunction, not an assignment.
          /\ undue' = (undue \/ (\E i \in eligible : ~due[i]))
    /\ phase' = [phase EXCEPT ![s] = "idle"]
    /\ plan' = [plan EXCEPT ![s] = {}]
    /\ UNCHANGED <<evInst, due, busy, archived>>

Next ==
    \/ \E i \in Instances : ToggleBusy(i) \/ TogglePolicy(i)
    \/ \E s \in Sweepers : Plan(s) \/ ArchiveOk(s) \/ ArchiveFailed(s) \/ Execute(s)

Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------

(***************************************************************************)
(* The reader's guarantee: nothing deleted sits above the floor, so a       *)
(* cursor at or above it has provably lost nothing — however scattered the  *)
(* deleted set, which it is, because retention removes whole instances.     *)
(***************************************************************************)
FloorCoversDeletions == \A p \in deletedEv : p =< floor

(***************************************************************************)
(* The other direction, and the one the plan/execute gap threatens: the     *)
(* floor must be something that was actually deleted. A floor above         *)
(* everything really removed truncates readers for nothing — the deletions  *)
(* would be safe and the readers would still get CursorTruncated.           *)
(***************************************************************************)
FloorIsSomethingDeleted == floor = 0 \/ floor \in deletedEv

(***************************************************************************)
(* "No archive, no deletion" — on every path, because `execute_retention`   *)
(* runs the sink itself rather than trusting its caller.                    *)
(***************************************************************************)
NothingDeletedWithoutArchive == gone \subseteq archived

(***************************************************************************)
(* The DUE predicate is re-applied under the row lock, so a policy changed  *)
(* during the archive gap stops the in-flight batch and not merely the next *)
(* one.                                                                     *)
(***************************************************************************)
OnlyDueRecordsDeleted == ~undue

(***************************************************************************)
(* An event is deleted only with its instance: the 0007 foreign key makes   *)
(* "an event never outlives its instance" the database's rule rather than   *)
(* this codebase's assertion.                                               *)
(***************************************************************************)
EventsGoWithTheirInstance ==
    \A p \in deletedEv : evInst[p] \in gone

=============================================================================
