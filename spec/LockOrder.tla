------------------------------ MODULE LockOrder ------------------------------
(***************************************************************************)
(* The engine's row-locking protocol, as a protocol — not as code.          *)
(*                                                                          *)
(* Design brief, phase 3: the originally sketched timer claim took the      *)
(* timer row first (DELETE ... FOR UPDATE SKIP LOCKED) and *then* the       *)
(* instance row, which is the opposite order from every other step path —   *)
(* "a classic AB/BA deadlock, survivable via Postgres's detector but        *)
(* needless". The shipped protocol instead takes the instance row first,    *)
(* with NOWAIT, then re-checks the timer row under that lock.               *)
(*                                                                          *)
(* Two transactions compete for two row locks per instance:                 *)
(*                                                                          *)
(*   completion  instance row (blocking), then the item/timer row           *)
(*   timer fire  instance row (NOWAIT -> give up), then the timer row       *)
(*                                                                          *)
(* HistoricalTimerOrder = TRUE restores the rejected sketch, so TLC can be  *)
(* made to find the deadlock on demand. That is the point: the ordering     *)
(* rule is backed by a counterexample, not by an assertion.                 *)
(*                                                                          *)
(* Checking the model separates two things the prose tends to conflate.     *)
(* Deleting GiveUp — making the timer claim block like everything else —    *)
(* does NOT reintroduce the deadlock: with a single lock order there is no  *)
(* cycle to find, whoever waits. Deadlock freedom comes from the *order*.   *)
(* NOWAIT buys something else, and `scheduler.rs` says which: an embedder's *)
(* `*_in_tx` transaction may hold an instance row for a long time, and the  *)
(* drain loop must move on to other instances' timers rather than park      *)
(* behind it. Order is the safety property; NOWAIT is throughput.           *)
(***************************************************************************)
EXTENDS Naturals, Sequences

CONSTANTS
    Nodes,                 \* engine processes; there is no leader
    Instances,             \* process instances, one row lock each
    NoOne,                 \* the free-lock marker
    HistoricalTimerOrder   \* TRUE = the AB/BA sketch the design rejected

ASSUME NoOne \notin Nodes

\* Two lockable rows per instance: the instance row, and the item/timer row
\* whose deletion commits with the step.
Resources == ({"inst"} \X Instances) \cup ({"item"} \X Instances)

VARIABLES
    lock,     \* Resources -> Nodes \cup {NoOne}
    pc,       \* Nodes -> 0 idle | 1 holds none | 2 holds first | 3 holds both
    kind,     \* Nodes -> {"complete", "timer"}
    target    \* Nodes -> Instances

vars == <<lock, pc, kind, target>>

\* The order in which a node takes its two locks.
Order(n) ==
    IF kind[n] = "timer" /\ HistoricalTimerOrder
        THEN <<"item", "inst">>
        ELSE <<"inst", "item">>

Res(n, j) == <<Order(n)[j], target[n]>>

\* A timer claim never waits: NOWAIT on the instance row, SKIP LOCKED on the
\* timer row. It gives the attempt up and defers instead of blocking.
FirstIsTry(n) == kind[n] = "timer"

TypeOK ==
    /\ lock \in [Resources -> Nodes \cup {NoOne}]
    /\ pc \in [Nodes -> 0..3]
    /\ kind \in [Nodes -> {"complete", "timer"}]
    /\ target \in [Nodes -> Instances]

Init ==
    /\ lock = [r \in Resources |-> NoOne]
    /\ pc = [n \in Nodes |-> 0]
    /\ kind \in [Nodes -> {"complete", "timer"}]
    /\ target \in [Nodes -> Instances]

\* Begin a transaction: pick what this node is doing and which instance.
Begin(n) ==
    /\ pc[n] = 0
    /\ \E k \in {"complete", "timer"}, i \in Instances :
        /\ kind' = [kind EXCEPT ![n] = k]
        /\ target' = [target EXCEPT ![n] = i]
    /\ pc' = [pc EXCEPT ![n] = 1]
    /\ UNCHANGED lock

AcquireFirst(n) ==
    /\ pc[n] = 1
    /\ lock[Res(n, 1)] = NoOne
    /\ lock' = [lock EXCEPT ![Res(n, 1)] = n]
    /\ pc' = [pc EXCEPT ![n] = 2]
    /\ UNCHANGED <<kind, target>>

\* NOWAIT / SKIP LOCKED: the row is taken, so give up rather than queue.
GiveUp(n) ==
    /\ pc[n] = 1
    /\ FirstIsTry(n)
    /\ lock[Res(n, 1)] # NoOne
    /\ pc' = [pc EXCEPT ![n] = 0]
    /\ UNCHANGED <<lock, kind, target>>

\* The second lock is always taken blocking: this action is simply not
\* enabled while another node holds the row. That is what can deadlock.
AcquireSecond(n) ==
    /\ pc[n] = 2
    /\ lock[Res(n, 2)] = NoOne
    /\ lock' = [lock EXCEPT ![Res(n, 2)] = n]
    /\ pc' = [pc EXCEPT ![n] = 3]
    /\ UNCHANGED <<kind, target>>

Commit(n) ==
    /\ pc[n] = 3
    /\ lock' = [r \in Resources |-> IF lock[r] = n THEN NoOne ELSE lock[r]]
    /\ pc' = [pc EXCEPT ![n] = 0]
    /\ UNCHANGED <<kind, target>>

Next == \E n \in Nodes :
    Begin(n) \/ AcquireFirst(n) \/ GiveUp(n) \/ AcquireSecond(n) \/ Commit(n)

\* Strong fairness per node, and deliberately NOT on Begin: nothing obliges a
\* node to start work, but once started it must finish. SF rather than WF
\* because a waiter's acquisition is only intermittently enabled (the row
\* frees, then is taken again) — SF is the honest abstraction of Postgres
\* queueing its lock waiters.
Fairness ==
    \A n \in Nodes :
        SF_vars(AcquireFirst(n) \/ GiveUp(n) \/ AcquireSecond(n) \/ Commit(n))

Spec == Init /\ [][Next]_vars /\ Fairness

-----------------------------------------------------------------------------

(***************************************************************************)
(* The engine's stated invariant: "the one lock order engine-wide           *)
(* (instance row, then item row)". Nobody ever holds an item/timer row lock *)
(* without already holding its instance row lock.                           *)
(***************************************************************************)
SingleLockOrder ==
    \A i \in Instances :
        lock[<<"item", i>>] # NoOne =>
            lock[<<"inst", i>>] = lock[<<"item", i>>]

(***************************************************************************)
(* Deadlock freedom is checked by TLC directly (a state with no successor). *)
(* This is the redundant, explicit form: no node is ever stuck holding one  *)
(* row while another node holds the row it needs and is itself blocked.     *)
(***************************************************************************)
Blocked(n) == pc[n] = 2 /\ lock[Res(n, 2)] # NoOne
NoWaitCycle ==
    ~ (\E n, m \in Nodes :
        /\ n # m
        /\ Blocked(n) /\ Blocked(m)
        /\ lock[Res(n, 2)] = m
        /\ lock[Res(m, 2)] = n)

(***************************************************************************)
(* Liveness: every node that begins a transaction eventually returns to     *)
(* idle — it commits, or gives up and defers. Nothing parks forever.        *)
(***************************************************************************)
EventuallyIdle == \A n \in Nodes : (pc[n] # 0) ~> (pc[n] = 0)

=============================================================================
