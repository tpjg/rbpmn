------------------------------ MODULE BoundaryExit ------------------------------
(***************************************************************************)
(* One token parked at a host work item, with one boundary subscription     *)
(* armed on it: a user task with an interrupting message boundary — the     *)
(* payment arriving while the ticket is being contested                      *)
(* (docs/design/boundary-messages.md §8). Two verbs can end that wait:       *)
(*                                                                          *)
(*   complete_task   lock the instance row, guard_lease reads the item,     *)
(*                   AlreadyClosed if it is not open, else step; the step   *)
(*                   withdraws the boundary's subscription row              *)
(*                   (cancel_attachments) in the same transaction.          *)
(*   correlate       resolve the subscription WITHOUT a lock, then lock the *)
(*                   instance row, re-check that THIS subscription is still *)
(*                   in the rehydrated state (NoSubscription if not), else  *)
(*                   step; the step cancels the host's work item.            *)
(*                                                                          *)
(* Any node may run either. The spec's sentence is "activity completion and *)
(* boundary triggering are mutually exclusive on one activation", and the   *)
(* engine earns it from two things, each with a counterexample config:      *)
(*   - completion withdraws the arm in its own transaction                  *)
(*     (`ArmDiesWithTheWait`; BoundaryExit_NoWithdraw.cfg drops it);        *)
(*   - delivery re-checks ITS row under the lock                            *)
(*     (`LateCallsAreTyped`; BoundaryExit_NoRecheck.cfg drops the re-check, *)
(*     BoundaryExit_AnyRowRecheck.cfg re-checks "some row" instead).        *)
(*                                                                          *)
(* What this model deliberately leaves out: the core's own defence. A       *)
(* delivery that reached `step` for a withdrawn subscription would get      *)
(* `StepError::UnknownSubscription` — an internal error, not a second exit. *)
(* Crediting that here would prove safety through a check the design does  *)
(* not intend to rely on; the point of the re-check is the *typed* answer   *)
(* (404, never 500), and `stepped` is what records that the re-check, not   *)
(* the core, was what stood between a late message and a closed task.       *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS
    Nodes,              \* engine nodes; any may complete or correlate
    Recheck,            \* TRUE = correlate re-checks under the lock (shipped)
    RowSpecificRecheck, \* TRUE = the re-check is for THIS row (shipped)
    OtherRow,           \* a subscription row of some OTHER token exists
    WithdrawOnComplete, \* TRUE = completion withdraws the arm (shipped)
    MaxLate             \* bound on recorded late answers, to stay finite

ASSUME Recheck \in BOOLEAN
ASSUME RowSpecificRecheck \in BOOLEAN
ASSUME OtherRow \in BOOLEAN
ASSUME WithdrawOnComplete \in BOOLEAN
ASSUME MaxLate \in Nat

VARIABLES
    item,        \* rbpmn_work_item.state: "open" | "completed" | "cancelled"
    armed,       \* the boundary's rbpmn_subscription row exists
    picked,      \* Nodes -> BOOLEAN: resolved the row without a lock
    completions, \* completions that reached step
    deliveries,  \* deliveries that reached step
    late,        \* typed late answers given (AlreadyClosed, NoSubscription)
    stepped      \* TRUE once step ran with its precondition false

vars == <<item, armed, picked, completions, deliveries, late, stepped>>

TypeOK ==
    /\ item \in {"open", "completed", "cancelled"}
    /\ armed \in BOOLEAN
    /\ picked \in [Nodes -> BOOLEAN]
    /\ completions \in Nat
    /\ deliveries \in Nat
    /\ late \in 0..MaxLate
    /\ stepped \in BOOLEAN

Init ==
    /\ item = "open"
    /\ armed = TRUE
    /\ picked = [n \in Nodes |-> FALSE]
    /\ completions = 0
    /\ deliveries = 0
    /\ late = 0
    /\ stepped = FALSE

\* complete_task, under the instance lock: guard_lease read the item open,
\* the step ran, and cancel_attachments withdrew the boundary's subscription
\* row in the same transaction — unless the buggy config keeps it.
Complete(n) ==
    /\ item = "open"
    /\ item' = "completed"
    /\ armed' = IF WithdrawOnComplete THEN FALSE ELSE armed
    /\ completions' = completions + 1
    /\ UNCHANGED <<picked, deliveries, late, stepped>>

\* ...and AlreadyClosed { state }: the item is not open, answered before the
\* core is invoked. Observable so that "a late call is answered typed" is a
\* transition TLC takes rather than one it cannot see; bounded to stay finite.
CompleteLate(n) ==
    /\ item # "open"
    /\ late < MaxLate
    /\ late' = late + 1
    /\ UNCHANGED <<item, armed, picked, completions, deliveries, stepped>>

\* correlate, first half: the unlocked resolve on the correlation index.
\* The window the whole race lives in.
Pick(n) ==
    /\ ~picked[n]
    /\ armed
    /\ picked' = [picked EXCEPT ![n] = TRUE]
    /\ UNCHANGED <<item, armed, completions, deliveries, late, stepped>>

\* The re-check as shipped asks whether THIS subscription is in the state
\* rebuilt under the lock. The two buggy shapes: no re-check at all, and a
\* re-check satisfied by any open subscription of the instance.
RecheckPasses ==
    IF ~Recheck THEN TRUE
    ELSE IF RowSpecificRecheck THEN armed
    ELSE armed \/ OtherRow

\* correlate, second half: instance row, re-check, step. An interrupting
\* delivery cancels the host's item and consumes the subscription row.
\* `stepped` records a step that should not have happened: the row was gone
\* or the item was already closed when the re-check let it through.
Deliver(n) ==
    /\ picked[n]
    /\ RecheckPasses
    \* Parenthesised on purpose: `=` binds tighter than `\/`, and written
    \* without them this is `(stepped' = stepped) \/ ~armed \/ ...`, which
    \* leaves stepped' unconstrained exactly when it should become TRUE —
    \* the mistake Retention.tla once shipped with `undue' = undue \/ X`.
    /\ stepped' = (stepped \/ ~armed \/ item # "open")
    /\ item' = IF item = "open" THEN "cancelled" ELSE item
    /\ armed' = FALSE
    /\ deliveries' = deliveries + 1
    /\ picked' = [picked EXCEPT ![n] = FALSE]
    /\ UNCHANGED <<completions, late>>

\* ...and NoSubscription: the re-check lost. The candidate is dropped and the
\* caller gets the 404 — the same answer a repeat of a delivered message gets.
DeliverLate(n) ==
    /\ picked[n]
    /\ ~RecheckPasses
    /\ late < MaxLate
    /\ late' = late + 1
    /\ picked' = [picked EXCEPT ![n] = FALSE]
    /\ UNCHANGED <<item, armed, completions, deliveries, stepped>>

Next == \E n \in Nodes :
    \/ Complete(n) \/ CompleteLate(n)
    \/ Pick(n) \/ Deliver(n) \/ DeliverLate(n)

Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------

(***************************************************************************)
(* Activity completion and boundary triggering are mutually exclusive on   *)
(* one activation: at most one exit ever reaches step. The two exits are   *)
(* the model's legitimate terminal states, which is why its configs run    *)
(* with -deadlock.                                                         *)
(***************************************************************************)
ExactlyOneExit == completions + deliveries <= 1

(***************************************************************************)
(* The TimerTeardown invariant on this path: completion withdraws the       *)
(* boundary's row in its own transaction, so an armed row always means an   *)
(* open host. Without it a PAID arriving after the contest was decided      *)
(* would interrupt a task that no longer exists.                           *)
(***************************************************************************)
ArmDiesWithTheWait == armed => item = "open"

(***************************************************************************)
(* After an exit, both verbs are answered typed and neither reaches step:  *)
(* complete_task gets AlreadyClosed, correlate gets NoSubscription. The     *)
(* re-check is what earns the second half; the core's UnknownSubscription  *)
(* would turn the same late delivery into an internal error instead.       *)
(***************************************************************************)
LateCallsAreTyped == stepped = FALSE

=============================================================================
