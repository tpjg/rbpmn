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
(* Interrupting = FALSE is the non-interrupting boundary: a delivery leaves *)
(* the host open, consumes its row and arms a FRESH row for the same key in *)
(* the same transaction. Nothing exits, so ExactlyOneExit is about          *)
(* completion alone there; the new question is the one a review found: a    *)
(* second delivery that resolved the consumed row meets the re-check        *)
(* failing while the re-armed row waits for exactly its message. The        *)
(* re-check answered 404 (`NoFalseNotFound`;                                *)
(* BoundaryExit_NonInterruptingNoReResolve.cfg). Shipped, a failed re-check *)
(* resolves once more under the lock and delivers to the row it finds, if   *)
(* that row is in the rehydrated state — still row-specific, so             *)
(* LateCallsAreTyped is unchanged.                                          *)
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
    Interrupting,       \* FALSE = a delivery re-arms instead of exiting
    ReResolve,          \* TRUE = a failed re-check resolves again (shipped)
    MaxRows,            \* bound on re-armed rows, to stay finite
    MaxLate             \* bound on recorded late answers, to stay finite

ASSUME Recheck \in BOOLEAN
ASSUME RowSpecificRecheck \in BOOLEAN
ASSUME OtherRow \in BOOLEAN
ASSUME WithdrawOnComplete \in BOOLEAN
ASSUME Interrupting \in BOOLEAN
ASSUME ReResolve \in BOOLEAN
ASSUME MaxRows \in Nat /\ MaxRows >= 1
ASSUME MaxLate \in Nat

VARIABLES
    item,        \* rbpmn_work_item.state: "open" | "completed" | "cancelled"
    armed,       \* the boundary has an rbpmn_subscription row
    row,         \* that row's subscription_no (a re-arm mints the next)
    picked,      \* Nodes -> the row resolved without a lock; 0 = none
    completions, \* completions that reached step
    deliveries,  \* deliveries that reached step
    late,        \* typed late answers given (AlreadyClosed, NoSubscription)
    stepped,     \* TRUE once step ran with its precondition false
    falseNotFound \* TRUE once a 404 was answered while a row waited

vars == <<item, armed, row, picked, completions, deliveries, late, stepped,
          falseNotFound>>

TypeOK ==
    /\ item \in {"open", "completed", "cancelled"}
    /\ armed \in BOOLEAN
    /\ row \in 1..MaxRows
    /\ picked \in [Nodes -> 0..MaxRows]
    /\ completions \in Nat
    /\ deliveries \in Nat
    /\ late \in 0..MaxLate
    /\ stepped \in BOOLEAN
    /\ falseNotFound \in BOOLEAN

Init ==
    /\ item = "open"
    /\ armed = TRUE
    /\ row = 1
    /\ picked = [n \in Nodes |-> 0]
    /\ completions = 0
    /\ deliveries = 0
    /\ late = 0
    /\ stepped = FALSE
    /\ falseNotFound = FALSE

\* complete_task, under the instance lock: guard_lease read the item open,
\* the step ran, and cancel_attachments withdrew the boundary's subscription
\* row in the same transaction — unless the buggy config keeps it.
Complete(n) ==
    /\ item = "open"
    /\ item' = "completed"
    /\ armed' = IF WithdrawOnComplete THEN FALSE ELSE armed
    /\ completions' = completions + 1
    /\ UNCHANGED <<row, picked, deliveries, late, stepped, falseNotFound>>

\* ...and AlreadyClosed { state }: the item is not open, answered before the
\* core is invoked. Observable so that "a late call is answered typed" is a
\* transition TLC takes rather than one it cannot see; bounded to stay finite.
CompleteLate(n) ==
    /\ item # "open"
    /\ late < MaxLate
    /\ late' = late + 1
    /\ UNCHANGED <<item, armed, row, picked, completions, deliveries, stepped,
                   falseNotFound>>

\* correlate, first half: the unlocked resolve on the correlation index.
\* The window the whole race lives in.
Pick(n) ==
    /\ picked[n] = 0
    /\ armed
    /\ picked' = [picked EXCEPT ![n] = row]
    /\ UNCHANGED <<item, armed, row, completions, deliveries, late, stepped,
                   falseNotFound>>

\* The re-check as shipped asks whether THIS subscription is in the state
\* rebuilt under the lock. The two buggy shapes: no re-check at all, and a
\* re-check satisfied by any open subscription of the instance.
RecheckPasses(n) ==
    IF ~Recheck THEN TRUE
    ELSE IF RowSpecificRecheck THEN armed /\ picked[n] = row
    ELSE armed \/ OtherRow

\* The shipped fallback: resolve again under the lock, and take the row found
\* only if it is in the rehydrated state — which, with the lock held, is the
\* boundary's current row.
ReResolves(n) == ~RecheckPasses(n) /\ ReResolve /\ armed

\* The row a delivery steps with: the one it picked, or the one it resolved
\* again.
Target(n) == IF RecheckPasses(n) THEN picked[n] ELSE row

\* correlate, second half: instance row, re-check, step. An interrupting
\* delivery cancels the host's item and consumes the subscription row; a
\* non-interrupting one consumes the row and re-arms the next, leaving the
\* host open. `stepped` records a step that should not have happened: the
\* row was gone or the item was already closed when the re-check let it
\* through.
Deliver(n) ==
    /\ picked[n] # 0
    /\ RecheckPasses(n) \/ ReResolves(n)
    /\ Interrupting \/ row < MaxRows
    /\ LET valid == armed /\ Target(n) = row /\ item = "open"
       \* Parenthesised on purpose: `=` binds tighter than `\/`, and written
       \* without them this is `(stepped' = stepped) \/ ~valid`, which
       \* leaves stepped' unconstrained exactly when it should become TRUE —
       \* the mistake Retention.tla once shipped with `undue' = undue \/ X`.
       IN /\ stepped' = (stepped \/ ~valid)
          /\ IF Interrupting
             THEN /\ item' = IF item = "open" THEN "cancelled" ELSE item
                  /\ armed' = FALSE
                  /\ row' = row
                  /\ deliveries' = deliveries + 1
             ELSE /\ item' = item
                  /\ armed' = armed
                  /\ row' = IF valid THEN row + 1 ELSE row
                  /\ deliveries' = deliveries
    /\ picked' = [picked EXCEPT ![n] = 0]
    /\ UNCHANGED <<completions, late, falseNotFound>>

\* ...and NoSubscription: the re-check lost. The candidate is dropped and the
\* caller gets the 404 — the same answer a repeat of a delivered message gets.
\* `falseNotFound` records a 404 answered while a row of this key waited
\* under the lock — the message had somewhere to go.
DeliverLate(n) ==
    /\ picked[n] # 0
    /\ ~RecheckPasses(n)
    /\ ~ReResolves(n)
    /\ late < MaxLate
    /\ late' = late + 1
    /\ falseNotFound' = (falseNotFound \/ armed)
    /\ picked' = [picked EXCEPT ![n] = 0]
    /\ UNCHANGED <<item, armed, row, completions, deliveries, stepped>>

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

(***************************************************************************)
(* The other half of "typed": a 404 is true. NoSubscription is answered    *)
(* only when nothing of this key waits under the lock — never because the  *)
(* row this call happened to resolve was consumed and re-armed.            *)
(***************************************************************************)
NoFalseNotFound == falseNotFound = FALSE

=============================================================================
