---------------------------- MODULE RepairClock ----------------------------
(***************************************************************************)
(* A repair moves a kept timer; the scheduler's claim must not fire it      *)
(* early (docs/design/incident-scope.md, D8).                               *)
(*                                                                          *)
(* The instance's clock stops while it is frozen: the repair that lands     *)
(* re-arms every timer the freeze kept (`resume_after_freeze`). A duration  *)
(* moves by the outage. A cycle occurrence that came due at or after        *)
(* `frozen_at` steps along its grid to the first occurrence at or after     *)
(* now; one due before it keeps its time and fires on resume.               *)
(*                                                                          *)
(* The claim path is `TimerTeardown`'s: pick a due row with NO lock held,   *)
(* then take the instance row NOWAIT and re-check. What is new is that a    *)
(* row picked while due can be moved later in the window, by a freeze and   *)
(* a repair both committing between the pick and the lock. For a duration   *)
(* it cannot happen — a row due at the pick moves by exactly the outage and *)
(* is still due at resume. For a cycle it can, at one instant: `frozen_at`  *)
(* is the freezing transaction's clock, taken before its commit makes the   *)
(* freeze visible, so a scheduler still reading the instance as active can  *)
(* pick an occurrence due at or after `frozen_at`, and the repair then steps *)
(* it past now. In discrete time that is the tick where `due = frozenAt`.   *)
(* So the re-check's `due_at <= now()` is load-bearing, and it is what      *)
(* RepairClock_NoDueRecheck.cfg removes.                                    *)
(*                                                                          *)
(* The scheduler's status check under the lock (`try_fire`'s               *)
(* `status != Active -> Resolved`) is kept in both configs: it is what      *)
(* stops a row picked before a freeze from firing while the instance is     *)
(* frozen, and it is not in question here.                                  *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS
    Kinds,      \* the rules a kept timer moves by: "duration", "cycle"
    Period,     \* a cycle's period
    MaxTime,    \* clock bound, to keep the model finite
    IgnoreDue   \* TRUE = the claim's re-check drops `due_at <= now()`

ASSUME Kinds \subseteq {"duration", "cycle"}
ASSUME Period \in Nat /\ Period > 0
ASSUME IgnoreDue \in BOOLEAN

VARIABLES
    now,       \* database time
    active,    \* the instance is not frozen
    frozenAt,  \* rbpmn_instance.frozen_at, set by the freeze
    kind,      \* which rule this timer moves by
    due,       \* rbpmn_timer.due_at
    armed,     \* the row exists (a fire deletes it)
    picked,    \* the scheduler holds it as an unlocked candidate
    early      \* history: a fire happened before its row was due

vars == <<now, active, frozenAt, kind, due, armed, picked, early>>

TypeOK ==
    /\ now \in 0..MaxTime
    /\ active \in BOOLEAN
    /\ frozenAt \in 0..MaxTime
    /\ kind \in Kinds
    /\ due \in Nat
    /\ armed \in BOOLEAN
    /\ picked \in BOOLEAN
    /\ early \in BOOLEAN

Init ==
    /\ now = 0
    /\ active = TRUE
    /\ frozenAt = 0
    /\ kind \in Kinds
    /\ due \in 0..MaxTime
    /\ armed = TRUE
    /\ picked = FALSE
    /\ early = FALSE

Tick ==
    /\ now < MaxTime
    /\ now' = now + 1
    /\ UNCHANGED <<active, frozenAt, kind, due, armed, picked, early>>

\* Something in the instance fails with nothing to catch it: it freezes, and
\* the freezing step stamps its clock.
Freeze ==
    /\ active
    /\ active' = FALSE
    /\ frozenAt' = now
    /\ UNCHANGED <<now, kind, due, armed, picked, early>>

\* ceil(a / b) for a >= 0, b > 0.
CeilDiv(a, b) == (a + b - 1) \div b

\* D8's move, as `resume_after_freeze` runs it: epoch arithmetic, and a cycle
\* stepped only when its occurrence came due at or after the freeze.
Moved ==
    CASE kind = "duration"                          -> due + (now - frozenAt)
      [] kind = "cycle" /\ due >= frozenAt /\ due < now
                                                    -> due + Period * CeilDiv(now - due, Period)
      [] OTHER                                      -> due

Repair ==
    /\ ~active
    /\ active' = TRUE
    /\ due' = IF armed THEN Moved ELSE due
    /\ UNCHANGED <<now, frozenAt, kind, armed, picked, early>>

\* The unlocked candidate scan: `due_at <= now() and i.status = 'active'`.
Pick ==
    /\ armed
    /\ active
    /\ due <= now
    /\ ~picked
    /\ picked' = TRUE
    /\ UNCHANGED <<now, active, frozenAt, kind, due, armed, early>>

\* NOWAIT gave up, the status check or the re-check turned it away.
Drop ==
    /\ picked
    /\ picked' = FALSE
    /\ UNCHANGED <<now, active, frozenAt, kind, due, armed, early>>

\* The claim under the instance lock: the instance active, the row still
\* there, and — the conjunct in question — still due. Firing deletes the row.
Fire ==
    /\ picked
    /\ active
    /\ armed
    /\ IgnoreDue \/ due <= now
    /\ early' = (early \/ now < due)
    /\ armed' = FALSE
    /\ picked' = FALSE
    /\ UNCHANGED <<now, active, frozenAt, kind, due>>

Next == Tick \/ Freeze \/ Repair \/ Pick \/ Drop \/ Fire

Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------

\* A timer fires at or after its due, a moved one included.
NeverFiresEarly == ~early

=============================================================================
