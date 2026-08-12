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
    RowOrder,              \* the per-instance rows a step touches, in order
    NoOne,                 \* the free-lock marker
    HistoricalTimerOrder   \* TRUE = the AB/BA sketch the design rejected

ASSUME NoOne \notin Nodes

Range(seq) == {seq[i] : i \in 1..Len(seq)}

\* Config files cannot carry tuple literals, so the concrete row lists live
\* here and the .cfg overrides `RowOrder` with one of them.
\* Two rows is enough to exhibit the ordering question and cheap enough to
\* check across several nodes and instances.
TwoRows == <<"item", "timer">>
\* Every per-instance row a step actually touches since phase 6. Checked in
\* LockOrderAllRows.cfg, which trades node and instance count for row count —
\* the arity question is within one instance, and the cross-instance question
\* is what the TwoRows configs cover.
AllRows == <<"token", "item", "timer", "subscription", "scope">>

\* The instance row, plus every per-instance row a step may touch. Since
\* phase 6 that is tokens, work items, timers, subscriptions AND scopes — so
\* `RowOrder` is a parameter rather than the single "item" this spec started
\* with, and the claim being checked is about *any* of them.
Resources == ({"inst"} \X Instances) \cup (Range(RowOrder) \X Instances)

VARIABLES
    lock,     \* Resources -> Nodes \cup {NoOne}
    pc,       \* Nodes -> 0 idle | 1 holds none | 2 holds first | 3 holds both
    kind,     \* Nodes -> {"complete", "timer"}
    target    \* Nodes -> Instances

vars == <<lock, pc, kind, target>>

\* The order in which a node takes its locks. Shipped: the instance row
\* first, then every per-instance row. Historical: the TIMER row first (the
\* rejected `DELETE ... FOR UPDATE SKIP LOCKED` claim), and only then the
\* instance row.
\*
\* "timer" is named rather than taken as Head(RowOrder): while RowOrder was a
\* single generic row that stood for the timer row, Head was faithful, but
\* once it grew to five named rows Head became "item"/"token" and the
\* counterexample deadlocked over a row the rejected design never touched
\* first.
OtherRows == SelectSeq(RowOrder, LAMBDA r : r # "timer")

Order(n) ==
    IF kind[n] = "timer" /\ HistoricalTimerOrder
        THEN <<"timer", "inst">> \o OtherRows
        ELSE <<"inst">> \o RowOrder

Last == Len(RowOrder) + 1

Res(n, j) == <<Order(n)[j], target[n]>>

\* A timer claim never waits: NOWAIT on the instance row, SKIP LOCKED on the
\* timer row. It gives the attempt up and defers instead of blocking.
FirstIsTry(n) == kind[n] = "timer"

TypeOK ==
    /\ lock \in [Resources -> Nodes \cup {NoOne}]
    /\ pc \in [Nodes -> 0..(Last + 1)]
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

\* Every subsequent lock is taken blocking: this action is simply not
\* enabled while another node holds the row. That is what can deadlock.
AcquireNext(n) ==
    /\ pc[n] \in 2..Last
    /\ lock[Res(n, pc[n])] = NoOne
    /\ lock' = [lock EXCEPT ![Res(n, pc[n])] = n]
    /\ pc' = [pc EXCEPT ![n] = pc[n] + 1]
    /\ UNCHANGED <<kind, target>>

Commit(n) ==
    /\ pc[n] = Last + 1
    /\ lock' = [r \in Resources |-> IF lock[r] = n THEN NoOne ELSE lock[r]]
    /\ pc' = [pc EXCEPT ![n] = 0]
    /\ UNCHANGED <<kind, target>>

Next == \E n \in Nodes :
    Begin(n) \/ AcquireFirst(n) \/ GiveUp(n) \/ AcquireNext(n) \/ Commit(n)

\* Strong fairness per node, and deliberately NOT on Begin: nothing obliges a
\* node to start work, but once started it must finish. SF rather than WF
\* because a waiter's acquisition is only intermittently enabled (the row
\* frees, then is taken again) — SF is the honest abstraction of Postgres
\* queueing its lock waiters.
Fairness ==
    \A n \in Nodes :
        SF_vars(AcquireFirst(n) \/ GiveUp(n) \/ AcquireNext(n) \/ Commit(n))

Spec == Init /\ [][Next]_vars /\ Fairness

-----------------------------------------------------------------------------

(***************************************************************************)
(* The engine's stated invariant: "the one lock order engine-wide           *)
(* (instance row, then item row)". Nobody ever holds an item/timer row lock *)
(* without already holding its instance row lock.                           *)
(***************************************************************************)
SingleLockOrder ==
    \A i \in Instances, k \in Range(RowOrder) :
        lock[<<k, i>>] # NoOne =>
            lock[<<"inst", i>>] = lock[<<k, i>>]

(***************************************************************************)
(* Deadlock freedom is checked by TLC directly (a state with no successor). *)
(* This is the redundant, explicit form: no node is ever stuck holding one  *)
(* row while another node holds the row it needs and is itself blocked.     *)
(***************************************************************************)
Blocked(n) == pc[n] \in 2..Last /\ lock[Res(n, pc[n])] # NoOne
NoWaitCycle ==
    ~ (\E n, m \in Nodes :
        /\ n # m
        /\ Blocked(n) /\ Blocked(m)
        /\ lock[Res(n, pc[n])] = m
        /\ lock[Res(m, pc[m])] = n)

(***************************************************************************)
(* Liveness: every node that begins a transaction eventually returns to     *)
(* idle — it commits, or gives up and defers. Nothing parks forever.        *)
(***************************************************************************)
EventuallyIdle == \A n \in Nodes : (pc[n] # 0) ~> (pc[n] = 0)

=============================================================================
