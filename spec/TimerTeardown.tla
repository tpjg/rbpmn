---------------------------- MODULE TimerTeardown ----------------------------
(***************************************************************************)
(* The scheduler's timer claim racing a scope teardown.                     *)
(*                                                                          *)
(* This models a bug that actually shipped (fixed in "Apply the phase-6     *)
(* review"): when an error was caught by a boundary on an *enclosing*       *)
(* subprocess, the failing task's token was removed before teardown ran, so *)
(* teardown — which only reaps tokens still present — never withdrew that   *)
(* token's armed boundary timers. The timer row outlived its token. The     *)
(* scheduler later picked it, re-checked it under the instance lock, found  *)
(* the row still there, fired it, and wedged the instance on an Invariant.  *)
(*                                                                          *)
(* The shape of the race is what matters:                                   *)
(*                                                                          *)
(*   Pick      a due timer is chosen with NO lock held (cheap index scan)   *)
(*   ...       a teardown may commit in this window                         *)
(*   Fire      the instance lock is taken and the timer row RE-CHECKED      *)
(*                                                                          *)
(* The re-check is the engine's defence, and modelling shows exactly what   *)
(* it is and is not worth: it verifies the timer ROW still exists, never    *)
(* that the row's TOKEN still exists. Those come apart precisely when       *)
(* teardown is incomplete — so the safety of the whole claim path rests on  *)
(* an invariant of teardown, not on the re-check.                           *)
(*                                                                          *)
(* BuggyTeardown = TRUE restores the shipped bug so TLC reproduces it.      *)
(*                                                                          *)
(* Abort models what DMN added to this path: the claim transaction can now  *)
(* roll back after the re-check, because the decision it evaluates lives in *)
(* the same transaction. See the action for why that changes nothing.       *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS
    Nodes,          \* schedulers and steppers; any node may do either
    Tokens,         \* runtime tokens of one instance
    Timers,         \* armed timer rows
    NoToken,        \* "this timer row is not armed"
    NoTimer,        \* "this node has not picked a candidate"
    BuggyTeardown   \* TRUE = teardown reaps tokens but not their timers

ASSUME NoToken \notin Tokens
ASSUME NoTimer \notin Timers

VARIABLES
    tokens,        \* the tokens that still exist
    timerToken,    \* Timers -> the token a timer is armed on, or NoToken
    picked,        \* Nodes -> the candidate chosen without a lock
    dangling       \* TRUE once a timer fired whose token was gone

vars == <<tokens, timerToken, picked, dangling>>

TypeOK ==
    /\ tokens \subseteq Tokens
    /\ timerToken \in [Timers -> Tokens \cup {NoToken}]
    /\ picked \in [Nodes -> Timers \cup {NoTimer}]
    /\ dangling \in BOOLEAN

Init ==
    /\ tokens = Tokens
    /\ timerToken = [t \in Timers |-> NoToken]
    /\ picked = [n \in Nodes |-> NoTimer]
    /\ dangling = FALSE

\* A step arms a boundary timer on a live token, in the same transaction that
\* parks it.
Arm(timer, token) ==
    /\ timerToken[timer] = NoToken
    /\ token \in tokens
    /\ timerToken' = [timerToken EXCEPT ![timer] = token]
    /\ UNCHANGED <<tokens, picked, dangling>>

\* The unlocked candidate scan: cheap on the due_at index, no lock held. This
\* is the window the whole race lives in.
Pick(n, timer) ==
    /\ picked[n] = NoTimer
    /\ timerToken[timer] # NoToken
    /\ picked' = [picked EXCEPT ![n] = timer]
    /\ UNCHANGED <<tokens, timerToken, dangling>>

\* NOWAIT gave up, or the re-check lost: drop the candidate and move on.
Drop(n) ==
    /\ picked[n] # NoTimer
    /\ picked' = [picked EXCEPT ![n] = NoTimer]
    /\ UNCHANGED <<tokens, timerToken, dangling>>

(***************************************************************************)
(* The claim transaction rolls back AFTER the re-check has already passed.  *)
(*                                                                          *)
(* New with DMN. `try_fire` used to run a pure `step` under the lock; it now *)
(* runs `step_answering_decisions`, which reads the definition's DMN from    *)
(* the database and evaluates it inside this same transaction. A decision    *)
(* that fails to compile aborts the transaction after the row was claimed    *)
(* and the re-check passed — a window that did not exist before.             *)
(*                                                                          *)
(* Modelled separately from Drop even though it leaves identical state, and  *)
(* that identity IS the finding: a rollback undoes the row deletion with     *)
(* everything else, so the claim is returned rather than consumed. Writing   *)
(* it as its own action is what turns "the abort is just a Drop" from an     *)
(* argument into something TLC checks. Note what it does NOT do — it does    *)
(* not clear timerToken. An abort that consumed the claim would silently     *)
(* lose a timer, and it is Postgres, not the engine, that rules that out.    *)
(***************************************************************************)
Abort(n) ==
    /\ picked[n] # NoTimer
    /\ timerToken[picked[n]] # NoToken          \* the re-check passed...
    /\ picked' = [picked EXCEPT ![n] = NoTimer]  \* ...and then it rolled back
    /\ UNCHANGED <<tokens, timerToken, dangling>>

(***************************************************************************)
(* Interrupting boundary / terminate: tear a scope down. One transaction    *)
(* under the instance lock, so it is atomic here.                           *)
(*                                                                          *)
(* Correct: reap the tokens AND withdraw every timer armed on them.         *)
(* Buggy:   reap the tokens only, leaving their timer rows behind.          *)
(***************************************************************************)
Teardown(doomed) ==
    /\ doomed \subseteq tokens
    /\ doomed # {}
    /\ tokens' = tokens \ doomed
    /\ timerToken' =
        IF BuggyTeardown
            THEN timerToken
            ELSE [t \in Timers |->
                    IF timerToken[t] \in doomed THEN NoToken ELSE timerToken[t]]
    /\ UNCHANGED <<picked, dangling>>

(***************************************************************************)
(* Claim and fire, under the instance lock. The re-check is exactly the     *)
(* engine's: does the timer ROW still exist? Firing deletes the row in the  *)
(* same transaction, which is what makes firing exactly-once.               *)
(***************************************************************************)
Fire(n) ==
    /\ picked[n] # NoTimer
    /\ timerToken[picked[n]] # NoToken          \* the re-check under the lock
    /\ dangling' = (dangling \/ timerToken[picked[n]] \notin tokens)
    /\ timerToken' = [timerToken EXCEPT ![picked[n]] = NoToken]
    /\ picked' = [picked EXCEPT ![n] = NoTimer]
    /\ UNCHANGED tokens

Next ==
    \/ \E timer \in Timers, token \in Tokens : Arm(timer, token)
    \/ \E n \in Nodes, timer \in Timers : Pick(n, timer)
    \/ \E n \in Nodes : Drop(n) \/ Abort(n) \/ Fire(n)
    \/ \E doomed \in SUBSET Tokens : Teardown(doomed)

Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------

(***************************************************************************)
(* The invariant teardown must preserve, and the one the whole claim path   *)
(* actually depends on: no timer row outlives the token it is armed on.     *)
(* The scheduler's re-check cannot substitute for this — it inspects the    *)
(* row, not the token.                                                      *)
(***************************************************************************)
ArmedTimersHaveLiveTokens ==
    \A t \in Timers : timerToken[t] # NoToken => timerToken[t] \in tokens

(***************************************************************************)
(* The consequence, stated separately because it is the observable failure: *)
(* the shipped bug fired a timer whose token was gone, which the core       *)
(* answers with StepError::Invariant — a wedged instance.                   *)
(***************************************************************************)
NeverFiredADanglingTimer == dangling = FALSE

=============================================================================
