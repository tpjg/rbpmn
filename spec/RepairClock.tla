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
(* then take the instance row NOWAIT and re-check. What is new is a row     *)
(* picked while due being moved later, and the window it happens in is the  *)
(* freeze's own. `persist_step` stamps `frozen_at` with `clock_timestamp()` *)
(* inside the freezing transaction, and the freeze is visible only once     *)
(* that transaction commits — which an embedder's `*_in_tx` transaction can *)
(* hold off indefinitely. Until then the scheduler reads the instance as    *)
(* active and can pick a row that came due after the stamp. The repair then *)
(* moves it by an outage measured from the stamp, past the resume: a        *)
(* duration by the whole outage, a cycle occurrence along its grid. So the  *)
(* claim's `due_at <= now()` re-check is what keeps a moved timer from      *)
(* firing early, and RepairClock_NoDueRecheck.cfg removes it.               *)
(*                                                                          *)
(* The freeze is therefore two steps here, Stamp and Commit: a freeze       *)
(* modelled as one atomic step has no such window, and concludes that a     *)
(* duration cannot be moved past now. The lock is modelled too: while the   *)
(* freezing transaction holds the instance row, the claim's NOWAIT gives up.*)
(* The status check under the lock (`try_fire`'s `status != Active ->       *)
(* Resolved`) is kept in every config; it is what stops a row picked before *)
(* a freeze from firing while the instance is frozen, and it is not in      *)
(* question here.                                                           *)
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
    phase,     \* "active"; "freezing": stamped, not committed; "frozen"
    frozenAt,  \* rbpmn_instance.frozen_at, stamped by the freeze
    kind,      \* which rule this timer moves by
    due,       \* rbpmn_timer.due_at
    armed,     \* the row exists (a fire deletes it)
    picked,    \* the scheduler holds it as an unlocked candidate
    early      \* history: a fire happened before its row was due

vars == <<now, phase, frozenAt, kind, due, armed, picked, early>>

TypeOK ==
    /\ now \in 0..MaxTime
    /\ phase \in {"active", "freezing", "frozen"}
    /\ frozenAt \in 0..MaxTime
    /\ kind \in Kinds
    /\ due \in Nat
    /\ armed \in BOOLEAN
    /\ picked \in BOOLEAN
    /\ early \in BOOLEAN

Init ==
    /\ now = 0
    /\ phase = "active"
    /\ frozenAt = 0
    /\ kind \in Kinds
    /\ due \in 0..MaxTime
    /\ armed = TRUE
    /\ picked = FALSE
    /\ early = FALSE

Tick ==
    /\ now < MaxTime
    /\ now' = now + 1
    /\ UNCHANGED <<phase, frozenAt, kind, due, armed, picked, early>>

\* Something in the instance fails with nothing to catch it. The freezing
\* transaction holds the instance row and stamps `frozen_at`...
Stamp ==
    /\ phase = "active"
    /\ phase' = "freezing"
    /\ frozenAt' = now
    /\ UNCHANGED <<now, kind, due, armed, picked, early>>

\* ...and only its commit makes the freeze visible.
Commit ==
    /\ phase = "freezing"
    /\ phase' = "frozen"
    /\ UNCHANGED <<now, frozenAt, kind, due, armed, picked, early>>

\* ceil(a / b) for a >= 0, b > 0.
CeilDiv(a, b) == (a + b - 1) \div b

\* D8's move, as `resume_after_freeze` runs it: epoch arithmetic, and a cycle
\* stepped only when its occurrence came due at or after the stamp.
Moved ==
    CASE kind = "duration"                          -> due + (now - frozenAt)
      [] kind = "cycle" /\ due >= frozenAt /\ due < now
                                                    -> due + Period * CeilDiv(now - due, Period)
      [] OTHER                                      -> due

Repair ==
    /\ phase = "frozen"
    /\ phase' = "active"
    /\ due' = IF armed THEN Moved ELSE due
    /\ UNCHANGED <<now, frozenAt, kind, armed, picked, early>>

\* The unlocked candidate scan, `due_at <= now() and i.status = 'active'`,
\* reads committed state: an uncommitted freeze still looks active.
Pick ==
    /\ armed
    /\ phase \in {"active", "freezing"}
    /\ due <= now
    /\ ~picked
    /\ picked' = TRUE
    /\ UNCHANGED <<now, phase, frozenAt, kind, due, armed, early>>

\* NOWAIT gave up, the status check or the re-check turned it away.
Drop ==
    /\ picked
    /\ picked' = FALSE
    /\ UNCHANGED <<now, phase, frozenAt, kind, due, armed, early>>

\* The claim under the instance lock — which NOWAIT only gets when no freeze
\* holds it — with the instance active, the row still there, and, the
\* conjunct in question, still due. Firing deletes the row.
Fire ==
    /\ picked
    /\ phase = "active"
    /\ armed
    /\ IgnoreDue \/ due <= now
    /\ early' = (early \/ now < due)
    /\ armed' = FALSE
    /\ picked' = FALSE
    /\ UNCHANGED <<now, phase, frozenAt, kind, due>>

Next == Tick \/ Stamp \/ Commit \/ Repair \/ Pick \/ Drop \/ Fire

Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------

\* A timer fires at or after its due, a moved one included.
NeverFiresEarly == ~early

\* D8's move is a move *forward*. The claim's re-check is what stops a moved
\* timer from firing early — and with it in place `early` cannot be set at
\* all, so `NeverFiresEarly` says nothing about the arithmetic that moved the
\* row. This does: an occurrence re-armed into the past would fire at once on
\* resume, and both configs would still reach their expected verdicts.
RepairNeverMovesATimerEarlier ==
    [][ (phase = "frozen" /\ phase' = "active") => due' >= due ]_vars

=============================================================================
