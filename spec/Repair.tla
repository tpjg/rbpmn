------------------------------- MODULE Repair -------------------------------
(***************************************************************************)
(* Repair: the one transition out of a frozen instance                     *)
(* (docs/design/incident-scope.md, D4–D9).                                  *)
(*                                                                          *)
(* Every other model treats the freeze as terminal. A repair takes the      *)
(* instance out of it, and two things have to be true of that step.         *)
(*                                                                          *)
(* It lands only on the incident it names. Operators send a request naming  *)
(* the open incident, and requests arrive at least once: a caller whose     *)
(* response was lost sends again. If the first landed as a Retry that       *)
(* failed again, the instance is frozen under the next number, and a resend *)
(* that landed there would repair an incident nobody looked at — or, as an  *)
(* abandon, terminate an instance someone has since repaired. `release_task`*)
(* shipped that bug with its lease, and `Lease`'s epoch fixed it; the       *)
(* incident number is the same fix, and                                     *)
(* RepairLandsOnlyOnTheIncidentItNamed the same property.                   *)
(*                                                                          *)
(* The sibling comes back whole. `LeaseSiblings_Stranded.cfg` prices the    *)
(* freeze: a sibling's open item, perhaps leased to a worker whose handler  *)
(* is running, can be neither claimed nor completed. A landed repair must   *)
(* pay that back — `ActiveStrandsNobody` — and must not cost the lease any  *)
(* of its own guarantees on the way: `active` has never gone from FALSE to  *)
(* TRUE in any model before this one.                                       *)
(*                                                                          *)
(* The sibling is `Lease` itself, instantiated as `LeaseSiblings` does it,  *)
(* so the lease checked here is the transcription the engine runs. The      *)
(* failing work is not modelled beyond its effect: its item closes as       *)
(* failed and nothing touches it again, because a repair never reopens a    *)
(* closed item (D5) — a Retry mints a new one.                              *)
(*                                                                          *)
(* UncheckedIncident = TRUE lands a request whenever the instance is        *)
(* frozen, whatever it names: Repair_UncheckedIncident.cfg.                 *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS
    Workers, NoOne, Process, NoLease, TTL, Backoff, Retries, MaxTime,
    MaxLeases, UncheckedRelease, EpochlessRelease, CompleteIgnoresClosed,
    Operators,         \* whoever sends repairs
    MaxIncidents,      \* bound on freezes, to keep the model finite
    NoIncident,        \* "this step landed no request"
    UncheckedIncident  \* TRUE = a request lands without its incident check

ASSUME MaxIncidents \in Nat /\ MaxIncidents > 0
ASSUME NoIncident \notin 0..MaxIncidents
ASSUME UncheckedIncident \in BOOLEAN

Kinds == {"repair", "abandon"}
Requests == [op : Operators, incident : 0..(MaxIncidents - 1), kind : Kinds]

VARIABLES
    active, now,
    sstate, sowner, suntil, sretryAt, sretries, sbelieves, scompletions,
    slastActor, sleaseNo, snamed, sissued,
    closed,     \* an abandon landed: the instance is terminated
    minted,     \* incidents raised so far; the open one is minted - 1
    sent,       \* the requests operators sent, each deliverable again
    landed,     \* the incident this step's landing request named, else NoIncident
    openAtThaw  \* history: a repair landed active while the sibling was open

sibling == <<sstate, sowner, suntil, sretryAt, sretries, sbelieves,
             scompletions, slastActor, sleaseNo, snamed, sissued>>
vars == <<active, now, sibling, closed, minted, sent, landed, openAtThaw>>

S == INSTANCE Lease WITH
    state <- sstate, owner <- sowner, until <- suntil, retryAt <- sretryAt,
    retries <- sretries, active <- active, now <- now, believes <- sbelieves,
    completions <- scompletions, lastActor <- slastActor,
    leaseNo <- sleaseNo, named <- snamed, issued <- sissued

Frozen == ~active /\ ~closed

\* The incident a frozen instance is at: each freeze mints the next number.
OpenIncident == minted - 1

TypeOK ==
    /\ S!TypeOK
    /\ closed \in BOOLEAN
    /\ minted \in 0..MaxIncidents
    /\ sent \subseteq Requests
    /\ landed \in 0..(MaxIncidents - 1) \cup {NoIncident}
    /\ openAtThaw \in BOOLEAN

Init ==
    /\ S!Init
    /\ closed = FALSE
    /\ minted = 0
    /\ sent = {}
    /\ landed = NoIncident
    /\ openAtThaw = FALSE

\* Something else in the instance fails with nothing to catch it: the
\* instance freezes, minting the next incident.
Freeze ==
    /\ active
    /\ minted < MaxIncidents
    /\ active' = FALSE
    /\ minted' = minted + 1
    /\ landed' = NoIncident
    /\ UNCHANGED <<now, sibling, closed, sent, openAtThaw>>

\* The sibling itself fails past its budget: the same freeze, its own item
\* closing as failed.
SiblingFreezes(w) ==
    /\ S!FailFinally(w)
    /\ minted < MaxIncidents
    /\ minted' = minted + 1
    /\ landed' = NoIncident
    /\ UNCHANGED <<closed, sent, openAtThaw>>

\* An operator reads the open incident — inspection shows its number — and
\* sends a request naming it. Sent once, it may arrive any number of times.
Send(o, k) ==
    /\ Frozen
    /\ sent' = sent \cup {[op |-> o, incident |-> OpenIncident, kind |-> k]}
    /\ landed' = NoIncident
    /\ UNCHANGED <<active, now, sibling, closed, minted, openAtThaw>>

\* The check under the instance lock, before anything changes: the instance
\* is frozen and the incident named is the open one.
Lands(r) == Frozen /\ (UncheckedIncident \/ r.incident = OpenIncident)

\* A repair lands: the instance goes active — or its disposition fails again
\* at once and freezes it under the next number, a Retry into the same
\* failure. Nothing of the sibling moves either way: its item was inert
\* through status alone, and becomes live again with no work (D7).
RepairLands(r) ==
    /\ r.kind = "repair"
    /\ Lands(r)
    /\ \/ /\ active' = TRUE
          /\ openAtThaw' = (openAtThaw \/ S!Open)
          /\ UNCHANGED minted
       \/ /\ minted < MaxIncidents
          /\ minted' = minted + 1
          /\ UNCHANGED <<active, openAtThaw>>
    /\ landed' = r.incident
    /\ UNCHANGED <<now, sibling, closed, sent>>

\* An abandon lands: the instance terminates, and the sibling's open item is
\* cancelled with everything else — a lease protects a worker from other
\* workers, never from the process (`Lease`'s Cancel, on a frozen instance).
AbandonLands(r) ==
    /\ r.kind = "abandon"
    /\ Lands(r)
    /\ closed' = TRUE
    /\ sstate' = IF S!Open THEN "cancelled" ELSE sstate
    /\ slastActor' = Process
    /\ snamed' = NoLease
    /\ landed' = r.incident
    /\ UNCHANGED <<active, now, sowner, suntil, sretryAt, sretries, sbelieves,
                   scompletions, sleaseNo, sissued, minted, sent, openAtThaw>>

\* Every other arrival — a stale number, an instance no longer frozen, a
\* resend of one that landed — is answered IncidentNotOpen and steps nothing.
Refused(r) ==
    /\ ~Lands(r)
    /\ landed' = NoIncident
    /\ UNCHANGED <<active, now, sibling, closed, minted, sent, openAtThaw>>

\* The sibling's own protocol, every action but FailFinally (paired with its
\* freeze above). Each one that needs the instance active says so itself.
SiblingStep ==
    /\ \/ S!Tick
       \/ S!Cancel
       \/ \E w \in Workers :
            \/ S!Acquire(w) \/ S!Extend(w) \/ S!ExtendLost(w)
            \/ S!ReleaseWith(w, sleaseNo) \/ S!ReleaseReplay(w) \/ S!ReleaseLost(w)
            \/ S!Complete(w) \/ S!CompleteRefused(w) \/ S!CompleteAlreadyClosed(w)
            \/ S!Fail(w) \/ S!FailCaught(w)
    /\ landed' = NoIncident
    /\ UNCHANGED <<closed, minted, sent, openAtThaw>>

Next ==
    \/ Freeze
    \/ \E w \in Workers : SiblingFreezes(w)
    \/ \E o \in Operators, k \in Kinds : Send(o, k)
    \/ \E r \in sent : RepairLands(r) \/ AbandonLands(r) \/ Refused(r)
    \/ SiblingStep

Spec == Init /\ [][Next]_vars

-----------------------------------------------------------------------------

(***************************************************************************)
(* D9: a repair or an abandon lands only on the incident it named. An       *)
(* action property, as `Lease`'s ReleaseFreesOnlyTheLeaseItNamed is and for *)
(* the same reason: a resend and a fresh request are the same request, and  *)
(* only what the landing step named — against what was open when it         *)
(* arrived — tells a stale one apart. Repair_UncheckedIncident.cfg drops the *)
(* check and TLC produces the trace: a repair of incident 0 fails again into *)
(* incident 1, and its resend lands there.                                  *)
(***************************************************************************)
RepairLandsOnlyOnTheIncidentItNamed ==
    [][ landed' # NoIncident => (Frozen /\ landed' = OpenIncident) ]_vars

\* The sibling keeps every lease guarantee across a thaw.
SiblingSafe ==
    /\ S!AtMostOneLiveHolder
    /\ S!CompletedAtMostOnce
    /\ S!NoLiveForeignCompletion
    /\ S!CancelledIsNeverCompleted

SiblingLeaseActions ==
    /\ S!LiveLeaseEndsOnlyByItsHolderOrTheProcess
    /\ S!NoCompletionAfterCancel
    /\ S!ReleaseFreesOnlyTheLeaseItNamed

\* The price of the freeze, paid back: once a repair lands the instance
\* active, the sibling is claimable or completable again.
ActiveStrandsNobody == active => S!NeverStranded

\* While frozen, nothing of the sibling moves but its holder handing it back
\* — until a request lands (an abandon cancels it).
FreezeAdvancesNothing ==
    [][ (Frozen /\ landed' = NoIncident) =>
          (sstate' # sstate => sstate = "locked" /\ sstate' = "available") ]_vars

\* An abandoned instance leaves nothing of its sibling open.
AbandonedLeavesNothingOpen == closed => ~S!Open

\* Deliberately FALSE — Repair_ThawIsReachable.cfg expects the violation. A
\* repair lands while the sibling is open, and the sibling then completes:
\* ActiveStrandsNobody is not vacuous over the case it exists for.
NoStrandedSiblingCompletes == ~(openAtThaw /\ sstate = "done")

=============================================================================
