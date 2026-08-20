# Message boundary events — design round

**Status: design only. No engine code in this round.** Fixture and spec
sketches are in the appendices, ready to drop in with the first slice; they
are deliberately *not* in the tree yet, because an accept fixture with a
message boundary fails today's corpus and an unreferenced `.tla` in `spec/`
would be a model nothing runs.

This round covers **message boundary events** — interrupting first,
non-interrupting second — and how they relate to the two v2 items the
open-items table already carries (non-interrupting boundary *timers*, and
`timeCycle`). It was read against `README.md`, `bpmn-engine-design.md`
(open-items table), `crates/rbpmn-model/src/lint/mod.rs` (the boundary
rejections, the host rules), `crates/rbpmn-core/src/compile.rs` (`ExecKind`,
message catch, boundary arming), `crates/rbpmn-core/src/step.rs` (arming,
delivery, teardown), `crates/rbpmn-engine/src/runtime.rs` (`correlate_in_tx`,
`load_instance`, `persist_step`), `scheduler.rs`, `tasks.rs`, migrations 0003
and 0012, and `spec/`.

---

## The motivating case, in one paragraph

Xilium, the first application, runs a *ticket*. A ticket can be contested; the
contest parks the instance at a user task `handle_contest`, claimed by a clerk
under a lease. A `PAID` message — correlated by the ticket reference — must be
able to arrive *while that task is open* and end the ticket as Paid: withdraw
the task, take the boundary path. Today a message is caught only at an
intermediate catch, a receive task, or an event-based gateway, so a payment
during a contest is refused with `NoSubscription` (HTTP 404) and the
application has to hold it somewhere and retry later. That is the interrupting
case, and it is the one that ships first. The secondary case — "every 7 days
while waiting for payment, add a late fee" — is modelled today as a loop that
re-arms an event-based gateway each cycle; a non-interrupting boundary timer
with `timeCycle` (`R/P7D`, or anchored `R/2026-08-27T00:00:00+02:00/P7D`)
says it directly. It is the same v2 item the brief already lists, and this
round sequences it behind the message boundary rather than beside it.

## Four things reading the code turned up

Worth stating before the design, because each one shaped a recommendation:

1. **The wait-state machinery already exists; this is a command arm, not a
   subsystem.** `Advancer::subscribe` resolves the correlation key at arm time,
   checks the key type, checks the duplicate rule and freezes on failure;
   `cancel_attachments` / `withdraw_arms` withdraw subscriptions as well as
   timers; `tear_down_scope` reaps a doomed token's subscriptions *with* it;
   `interrupt_to_boundary` does the scope teardown and takes the boundary
   path. A message boundary is `subscribe` called from `arm_boundaries`, plus a
   `DeliverMessage` arm per host wait kind that mirrors `FireTimer`'s.
2. **A latent loader bug the feature would trip.** `load_instance`
   (`runtime.rs`) resolves a token with `wait_kind = 'message'` to its
   subscription by `token_no` alone — "a token waiting on its own
   subscription has exactly one". A receive task with a message boundary has
   **two** subscriptions on one token, and the loader would take the lowest
   `subscription_no` — the host's, because `enter` arms the host before its
   boundaries. It would work by arm order, not by intent, and a non-
   interrupting re-arm or any future reordering would break it silently. It
   must resolve by `(token_no, element_id = the token's element)`, and the
   fsck must assert the uniqueness that makes that sound.
3. **A dead arm that lint accepts today.** `is_supported_boundary_host`
   includes the business-rule task, and `enter` arms its timer boundaries on a
   `WaitKind::Decision` token — which is answered, and its arms withdrawn,
   inside the same transaction. A timer boundary on a business-rule task
   produces `timer-armed` / `timer-cancelled` in one step and can never fire.
   That is exactly the "seems to run" the linter exists to kill. The
   recommendation below is to reject it; it is retroactively stricter, and §7
   says what that costs.
4. **`Lease.tla` has no process-driven cancellation.** Its only exits from a
   claim are the holder's verbs and the clock. Terminate, scope teardown and
   the interrupting timer boundary already cancel a leased item today, and none
   of them is in the model — so "what interrupting means for a leased work
   item" has been true by code, not by check. The exactly-once property the
   brief asks for belongs there, as a `Cancel` action.

---

## 0. Recommendations at a glance

| Decision | Recommendation |
|---|---|
| Hosts for a message boundary | service, user, receive task, embedded subprocess. **Not** the business-rule task — and *remove* the business-rule task from boundary hosts altogether (finding 3) |
| What "interrupting" means under a lease | the process may withdraw a leased item at any time; the lease only protects a holder from *other workers*. The holder learns at its next verb: `complete_task` → `AlreadyClosed{state: "cancelled"}`, `fail_task` → the same, `extend_lock` → `Lost`, `release_task` → `Lost`. Never success, never an error |
| Where the patch lands on an interrupting delivery | applied before the host is cancelled, as `DeliverMessage` already does; the holder's pending decision is discarded, and the typed result says so |
| Correlation binding | `Bindings::correlation(boundary_element_id, "ticket.reference")` — the boundary's own id, checked at L2 by `message-has-correlation` exactly like a catch. Nothing in the XML |
| Duplicate `(message, key)` | unchanged runtime rule (freeze). Plus a new **L2** rule `ambiguous-message-arm`⁺ for the statically certain cases (same host, host-vs-boundary, subprocess-boundary vs inner catch, with the *same* binding name) |
| Non-interrupting boundary paths | a new L1 rule `boundary-side-path`⁺: the path is a *side path* — it ends at its own end event and never merges into anything; the region analysis ignores non-interrupting pseudo-edges |
| `timeCycle` | only on a non-interrupting boundary; `R[n]/P…` and `R[n]/<datetime>/P…`; fixed-length periods (no months/years); the anchor fixes the *phase*, the first due is the first occurrence at or after arm time, no catch-up; re-arm from the previous due, not from fire time |
| Core shape for cycles | the core owns `remaining`, decides re-arm, and emits `TimerArmed { continues: Some(prev_id) }`; the projection computes every instant. No instants in the core |
| Projection | no new tables, no new wait kinds, no migration for slice 1. `rbpmn_subscription.element_id` = the boundary's id. Loader fix (finding 2). Migration 0013 only with cycles |
| Lock order | unchanged; `correlate` keeps its pick → instance row → re-check shape. Re-read `spec/`, re-run `just tla` |
| TLA+ | `Lease.tla` gains `Cancel`; new `BoundaryExit.tla` proves exactly-one-exit for correlate vs complete_task on one token (sketch in Appendix C); `TimerTeardown.tla`'s invariant is re-stated for subscription rows |
| HTTP | `POST /v1/messages` unchanged in every respect |
| Staging | slice 1 = interrupting message boundary on **all four hosts** (the brief's user+receive minimum costs more code than it saves — §10); slice 2 = non-interrupting message; slice 3 = non-interrupting timer + `timeCycle` |

---

## 1. Semantics

### 1.1 What BPMN 2.0 says

From the execution semantics (BPMN 2.0.2, clause 13, *Activity* and the
boundary-event rows of the event tables), in rbpmn's vocabulary:

- A boundary event is *attached to* an activity and is **active exactly while
  its host is active**: armed when the host is entered, disarmed when the host
  completes, is interrupted, or its scope is torn down. It has no incoming
  sequence flow and exactly one outgoing flow (the linter already enforces
  both; `structure.rs`).
- **Interrupting** (`cancelActivity="true"`, the XML default): when it
  triggers, the host is cancelled — a task's work item is withdrawn, a
  subprocess's whole scope is torn down — and *the host's token* continues on
  the boundary's outgoing flow. The host never completes. Every other boundary
  on the host is disarmed with it.
- **Non-interrupting** (`cancelActivity="false"`): when it triggers, the host
  keeps running and a **new token** is produced on the boundary's outgoing
  flow, in the host's container (the same scope as the host's token). The
  boundary **stays armed** for as long as the host is active, so it can
  trigger again: a message boundary can catch several messages, a cyclic
  timer fires on every occurrence. A non-cyclic timer fires once.
- A **message boundary** carries a `messageRef` and catches a message
  delivered to the process instance while the host is active. How the message
  finds the instance is not the spec's business; here it is
  `correlate(message, key)`, exactly as for every other catch.
- Activity completion and boundary triggering are **mutually exclusive** on
  one activation: one or the other, never both. That sentence is the property
  §8 proves.

### 1.2 Which hosts, and why

| Host | Token's wait kind while active | Interrupting message boundary | Non-interrupting |
|---|---|---|---|
| user task | `WorkItem` | withdraw the item (`work-item-cancelled`), take the boundary path | spawn a sibling; item untouched |
| service task | `WorkItem` | same — and a pull-mode external worker holding a lease gets the same typed answers a user-task frontend does | same |
| receive task | `Message(host sub)` | withdraw the host's subscription (`subscription-cancelled`), take the path — the two-subscriptions-on-one-token case (finding 2) | spawn a sibling; host subscription untouched |
| embedded subprocess | `Scope(child)` | `tear_down_scope(child)` recursively, then the path in the *parent* scope — `interrupt_to_boundary` already does this for timers | spawn a sibling **in the parent scope**, beside the parked subprocess token |
| business-rule task | `Decision` — transient | **rejected** (finding 3): the token is answered inside the transaction that parked it; no delivery can ever find it | rejected |
| event-based gateway targets | — | stay rejected by `event-gateway-structure`: the target is never *entered*, so a boundary on it would silently not exist |

### 1.3 What "interrupting" means for a leased work item

A lease is a **row value**, never a lock (`tasks.rs`, the design brief's
"lease model, not long locks"). It exists so that *another worker* cannot take
or complete an item somebody is demonstrably working on. It was never a
promise that the *process* would wait: terminate cancels open items under live
leases today, so does scope teardown, so does the interrupting timer boundary
(`boundary_timer_interrupts_the_task` asserts the row ends `cancelled`). A
message boundary adds nothing new to that contract except a new cause — one a
human can trigger by paying.

What the holder sees, each a typed result and none of them a 5xx:

| Holder's next verb | Engine result | Server rendering today |
|---|---|---|
| `complete_task(id, owner, patch)` | `Completion::AlreadyClosed { state: "cancelled" }` — the patch is **not** applied | `200 {"outcome":"alreadyClosed","state":"cancelled"}` |
| `fail_task` | `FailOutcome::AlreadyClosed { state: "cancelled" }` | same shape |
| `extend_lock` | `LockExtension::Lost` | `409 {"outcome":"lockLost"}` |
| `release_task` | `Released::Lost` | `409 {"outcome":"lockLost"}` |

Two things to say plainly to application authors, because Xilium will meet
both on day one:

- **The clerk's decision is discarded.** A `complete_task` that arrives after
  the `PAID` boundary fired returns `AlreadyClosed` and records nothing. If
  the application wants the lost decision kept, that is the application's
  write, made when it sees that outcome — the engine will not invent a place
  for it. "Never succeed" means exactly that: the patch never reaches the
  document.
- **The holder finds out at its next heartbeat.** Nothing pushes a
  cancellation to a lease holder (pull model; the holder may be a browser
  tab). The detection bound is the client's renewal interval, which is the
  same bound the lease already puts on everything else.

`LockExtension::Lost` and `Released::Lost` do not say *why*. For a timer
boundary "reassigned" was an adequate story; for a payment it is the wrong
one ("your task was reassigned" when the truth is "the ticket was paid"). A
follow-up, not part of any slice here: `Lost { state }`, carrying the row
state the way `AlreadyClosed` already does. Additive, no contract change.

### 1.4 The Xilium walk-through

Model (full fixture in Appendix A): `start → handle_contest (user task) →
end_decided`, with an interrupting message boundary `paid_during_contest`
(`messageRef` → message named `PAID`) on `handle_contest`, leading to
`end_paid`. Bindings: `{"correlations": {"paid_during_contest":
"ticket.reference"}}`. Variables at start: `{"ticket": {"reference":
"T-2026-0042"}}`.

1. `start` → the clerk's task is created, **and in the same step** the
   boundary's subscription is armed with the key evaluated now:
   `message-subscribed paid_during_contest PAID T-2026-0042`.
2. A clerk claims `handle_contest` (lease, 10 min, renewing).
3. `POST /v1/messages {"name":"PAID","correlationKey":"T-2026-0042","patch":
   {"payment":{"amount":60}}}` → `correlate` resolves the subscription
   (it is a row like any other; the index does not care that its element is a
   boundary), locks the instance row, re-checks, and steps
   `DeliverMessage`: the patch lands, the work item is cancelled, the boundary
   path runs to `end_paid`, the instance completes. `200 {"instanceId": …}`.
4. The clerk clicks *Uphold contest* → `complete_task` → the instance row is
   locked, `guard_lease` reads the item: `cancelled` → `AlreadyClosed`. The UI
   says the ticket was paid meanwhile.

If instead the clerk completes first: step 4 runs first and
`cancel_attachments` withdraws the `PAID` subscription
(`subscription-cancelled paid_during_contest PAID`); the payment in step 3
then finds no subscription → 404, which is the correct, loud answer — the
application decides what a payment against a decided contest means (usually:
the next wait state, `await_payment` after a rejected contest, is a catch for
the *same* `PAID` message, armed in the same transaction the contest closed,
so a payment a moment later is delivered there). Golden traces for both
orders are in Appendix B.

---

## 2. Lint

The rule of the timer-expression round holds here verbatim: **a lint relaxes
in the same change that makes the element executable, never before.** So the
three relaxations below land with their slices (§10), and the phase pointer
`NotYetExecutable` stands for anything still refused.

### 2.1 Relaxations (one per slice)

| Today (`lint/mod.rs`, the `NodeKind::Boundary` arm) | After |
|---|---|
| `BoundaryTrigger::Message(_)` → `no-unsupported-element` "message boundary events are not supported in v1" | slice 1: `check_message(defs, id, message_ref, out)` — the same XML-side check every message element gets: a `messageRef` resolving to a *named* message (`message-has-correlation`) |
| `!b.cancel_activity` → `no-unsupported-element` "non-interrupting boundary events are not supported in v1 (planned for v2)" | slice 2 (message) / slice 3 (timer): accepted, subject to `boundary-side-path` below |
| `TimerSpec::Cycle(_)` → `no-unsupported-element` "repeating timer cycles … planned for v2" | slice 3: accepted **on a non-interrupting boundary only**, validated by a new `iso8601::validate_cycle`. Anywhere else it stays `no-unsupported-element`, with the message rewritten: *a repeating timer is only executed on a non-interrupting boundary — on an interrupting boundary or an intermediate catch the first occurrence ends the wait, so write a `timeDuration`* (other engines fire once and silently drop the repetitions; rbpmn says no instead) |

### 2.2 Existing rules, and what each does with a boundary message

- **`message-has-correlation`** — two halves, unchanged in shape. XML half
  (L1): the boundary references a named message. Manifest half (L2, in
  `compile`): `bindings.correlations` must hold the **boundary's own element
  id**; the compile closure `correlation(node)` is called for the boundary node
  exactly as for a catch, and a missing entry joins `MissingCorrelation`. **XML
  purity stays absolute**: the XML says *a `PAID` message is caught here*; the
  manifest says *correlated by `ticket.reference`*.
- **`boundary-on-supported-host`** — message boundaries on service/user/
  receive tasks and subprocesses. New rejection under the same id: *any*
  timer or message boundary on a business-rule task, with the reason ("a
  decision completes inside the transaction that starts it; a boundary here
  can never fire"). Implementation: drop `BusinessRuleTask` from
  `is_supported_boundary_host`; the error-boundary clause already excludes it.
- **`event-gateway-structure`** — unchanged. A boundary (any kind) on a
  gateway alternative is still refused, and the message is still right.
- **`balanced-gateways`** — an *interrupting* boundary path is part of its
  host's branch through the host→boundary pseudo-edge (`structure.rs`,
  `regions.rs`) and must merge back before the join or terminate. Nothing
  changes for slice 1. Non-interrupting boundaries are the one case where the
  pseudo-edge model is **wrong**, handled by the new rule below.
- **`no-implicit-split`** — unaffected. It counts `flow_out` of the activity;
  the boundary's flow sources at the boundary, not the host.
- **`implicit-merge-after-parallel`** — unaffected for interrupting
  boundaries. For non-interrupting ones the side-path rule is stricter than
  this warning and pre-empts it.
- **`bpmn-structure`** — the existing "second boundary for the same error code
  can never fire" check has no exact message analogue at L1, because two
  `PAID` boundaries with *different* correlation keys are legal BPMN and could
  both fire. The certain cases move to L2 (§2.4).

### 2.3 New L1 rule: `boundary-side-path`⁺ (error) — slice 2

A non-interrupting boundary produces a *second* token in the host's scope. If
that token can reach a parallel join, the join receives two tokens on one
incoming flow and the core answers with the `Invariant` the mutation tests
already show for `cross-branch-merge`. If it can merge into the host's
continuation, the downstream activity runs twice — the "task runs twice"
trap, silently. Block structure was proven for tokens that enter through the
split; a side token enters nowhere.

**Rule.** Let `B` be a non-interrupting boundary and `P` the set of nodes
reachable from `B` over sequence flows (plus the pseudo-edges of boundaries
attached to activities in `P`). Every node in `P \ {B}` must have **all** its
predecessors (sequence flows and host pseudo-edges) inside `P ∪ {B}`. In words:
the side path is **disjoint from everything else in the scope**. It ends at
its own end event(s) — a plain end is *required*, because that is where the
side token is consumed; a terminate end is *allowed*, because "on the fifth
reminder, cancel the whole thing" is a legitimate escape and scope-local
terminate already exists. It may contain its own split/join blocks,
subprocesses and boundaries; those are checked as usual inside `P`.

Message, for the modeller: *a non-interrupting boundary starts a side path
that must end on its own — it cannot rejoin the flow after `handle_contest`
(that would run the rest of the process twice) or reach a parallel join (it
would deliver a second token). If you want "remind, then wait again", use an
interrupting boundary and a loop.*

**What the region analysis must change.** `Graph` carries `boundaries[host]`
as one list; it needs to know which are interrupting. `regions.rs` must walk
**only interrupting** pseudo-edges: a side path is not part of any branch, it
delivers nothing to any join, and its end events are not "a plain end inside
the region". `structure.rs`'s connectivity check keeps *all* pseudo-edges, so
side-path nodes are still reachable from the start and are not flagged as
orphans. Because `regions::check` only runs on an error-free scope and
`boundary-side-path` is an error, the region analysis may assume side paths
are disjoint.

This rule has a counterexample, not just a rationale: `side-path-into-join`
(Appendix A) run through `compile_without_lint` must produce the
`second token arrived at join` invariant, and the mutation test should say so.

### 2.4 New L2 rule: `ambiguous-message-arm`⁺ (error) — slice 1

The runtime duplicate rule — a second open subscription for the same
`(message, key)` in one instance freezes the instance — is the backstop, and
it stays exactly as it is. Boundaries make some duplicates **statically
certain**, and a certain freeze should be a deploy error rather than an
incident. With bindings in hand (`compile`, reported through
`check_deployable` like `decision-has-binding`), reject when two message arms
name the **same message and the same correlation binding** and can be live on
one instance at once:

1. two message boundaries on one host;
2. a message boundary on a receive task catching the host's own message;
3. a message boundary on a subprocess, and a message catch / receive task /
   message boundary of the same message anywhere inside its body — the
   parent's arm is live for the whole life of the inner scope, and
   connectivity guarantees the inner element is reachable.

Why L2 and not L1: with *different* correlation names the arms resolve to
different keys and both may legitimately be live; only the binding decides,
and the binding is not in the XML. Why not warn at L1 on the message name
alone: a rule id carries one severity, and a warning about a case that is
*certain* whenever the bindings coincide would be a weaker statement than the
one available. The bpmnlint plugin and the standalone linter (L1 only) will
not see this rule; they lint for other engines too, where the model is legal.

Not covered statically, by design: two arms for the same `(message, binding)`
on **parallel branches** of one region. The region analysis knows the branches
and could refuse it; that is an extension for when a fixture asks for it. The
runtime freeze catches it meanwhile, loudly, at arm time — never as a lost
message.

### 2.5 `timer-iso8601` for cycles — slice 3

Accepted grammar (a deliberate subset of ISO 8601 recurring intervals):

```
R[n]/P…                          n fires, period from arm time
R[n]/<datetime with offset>/P…   n fires, phase anchored at the datetime
```

- `n` absent = unbounded (bounded by the host's life, which is why it is only
  allowed on a non-interrupting boundary). `R0/…` is an error: it never fires.
- The period must be **fixed-length**: weeks, days, hours, minutes, seconds.
  `P1M` and `P1Y` are errors under `timer-iso8601` ("a repeating period must
  have a fixed length; months and years do not"). The projection computes
  occurrences with epoch arithmetic, and a month is not a number of seconds.
- `R/<start>/<end>` and `R/P…/<end>` forms are errors ("rbpmn accepts
  `Rn/P…` and `Rn/<datetime>/P…`").
- The datetime, if present, needs an explicit UTC offset, as every `timeDate`
  already does.
- The variable form works the same way as for durations and dates: text that
  is not a valid cycle but is a FEEL qualified name is read from the variable
  document at arm time and warned about under `timer-expression`
  (`TimerKind::Cycle`), and a bad value at arm time is a `timer-resolve-failed`
  incident.

Semantics of the anchor — decided here, and different from a strict reading
of ISO 8601, so said out loud: **the anchor fixes the phase, not the set of
instants.** The first due instant is the first `anchor + k·period ≥ arm
time`; `n` counts fires from there; occurrences before arm time are never
replayed (no catch-up burst). A strict reading makes `R3/2026-08-27…/P7D`
three fixed instants, which would turn every instance started after 2026-09-10
into one whose boundary silently never arms — a definition outlives its
anchor, and "every Monday at 00:00 local" is what a modeller means by an
anchored cycle. Daylight-saving caveat, also said out loud: periods are
fixed-length, so `P1D` anchored at `00:00+02:00` drifts an hour after the
clocks change. Calendar-aware schedules are not in this round.

### 2.6 Catalogue additions

| Rule | Severity | Tier | Meaning |
|---|---|---|---|
| `boundary-side-path` ⁺ | error | L1 | A non-interrupting boundary starts a side path: it ends at its own end event and never merges into another flow or reaches a parallel join. |
| `ambiguous-message-arm` ⁺ | error | L2 | Two message arms for the same message *and* the same correlation binding can be live at once (same host, host vs. its boundary, subprocess boundary vs. an inner catch): every delivery would be ambiguous, so deploy refuses it. |

Plus new messages under existing ids: `boundary-on-supported-host`
(business-rule task), `no-unsupported-element` (cycle outside a
non-interrupting boundary), `timer-iso8601` (cycle grammar). Both new ids are
stable from the day they land; neither is ever renamed.

---

## 3. The pure core

### 3.1 Compile

- `ExecKind::MessageBoundary { message: String, key: Vec<String>,
  interrupting: bool }`, entered only by its subscription being delivered —
  `enter` on it is an `Invariant`, like `TimerBoundary` today.
- `ExecKind::TimerBoundary` gains `interrupting: bool`; `TimerSource` /
  `TimerDue` gain a `Cycle` shape carrying the literal text and the parsed
  `repeats: Option<u32>` (slice 3).
- The per-host table `timer_boundaries: BTreeMap<NodeIx, Vec<NodeIx>>` becomes
  `boundaries` holding both kinds, **in declaration order** — arming order
  allocates ids and the golden traces pin it, so one list in XML order is the
  only deterministic choice. `error_boundaries` stays separate (matched by
  code, never armed).
- `subscribe` matches on `ExecKind::MessageCatch` today; it needs an accessor
  `message_arm(node) -> Option<(&str, &[String])>` that answers for both
  `MessageCatch` and `MessageBoundary`, and nothing else about it changes.
- Host validation mirrors the timer arm (`compile.rs`, the `boundary_hosts`
  loop): `Task { .. } | MessageCatch { .. } | SubProcess { .. }`, with the
  business-rule task excluded by lint *and* re-checked here as an `Internal`
  error, like every other "survived lint" guard.
- `CompileError::AmbiguousMessageArm { elements, message, binding }` for §2.4,
  mapped to `rule::AMBIGUOUS_MESSAGE_ARM` in `check.rs` beside
  `MissingCorrelation`.

### 3.2 Arming: a token waiting at an activity

`arm_boundaries(state, token, host)` runs today after the host's token is
parked — for a task after the work item is recorded, for a receive task after
the host's own subscription, for a subprocess before the child's first move is
queued. It walks `boundaries(host)` and calls `arm_timer` or `subscribe`
accordingly; the `#[must_use] bool` contract is unchanged: `false` means a
freeze happened and the caller stops.

What that buys without new code:

- **Correlation-key resolution at arm time** — `subscribe` evaluates the
  binding's qualified name against the variables **now**, when the host is
  entered. A missing or non-string/non-integer value is `correlation-failed`
  and the freeze parks the token **at the boundary element** (the same place a
  `timer-resolve-failed` boundary parks), the host's just-created work item is
  closed by `freeze`, and the instance is `failed`. Inspection reads "incident
  at `paid_during_contest`: `correlation-failed ticket.reference`", which is
  the right diagnosis.
- **The duplicate rule** — `subscribe` refuses a second open `(message, key)`
  in the instance and freezes. With boundaries this matters in one new way
  worth a sentence in the docs: *sequential* arms for the same message are
  fine (the `PAID` boundary on `handle_contest` is withdrawn in the transaction
  that closes the contest, and the `PAID` catch at `await_payment` is armed in
  that same transaction — there is never a moment with both), while
  *concurrent* ones — a parallel branch waiting for `PAID` at an event gateway
  while another branch's task carries a `PAID` boundary — freeze at the second
  arm. §2.4 catches the certain cases at deploy.
- **Arm order** — host's own arm first (work item / subscription), then
  boundaries in declaration order. Subscription ids and timer ids follow.

### 3.3 Delivery: the `DeliverMessage` arm, by host wait kind

`Command::DeliverMessage { id, patch }` already removes the subscription,
emits `message-received`, applies the patch (`variables-patched` if
non-empty), and then looks at the token the subscription pointed to. Two arms
exist (`Message(sid) if sid == id`, `EventGateway`). The new ones mirror
`FireTimer` line for line, which is the point — one shape for "an arm on a
parked token fired":

| Token's wait | Today | Interrupting boundary (slice 1) | Non-interrupting (slice 2) |
|---|---|---|---|
| `WorkItem(wid)` | `Invariant` | close the item, `work-item-cancelled host`; `interrupt_to_boundary(token, sub.element)` | spawn sibling (§3.5); re-arm |
| `Message(sid)`, `sid ≠ id` | `Invariant` | remove `sid`, `subscription-cancelled host <msg>`; `interrupt_to_boundary` | spawn sibling; re-arm |
| `Scope(child)` | `Invariant` | `interrupt_to_boundary` (it tears `child` down recursively and continues in the parent scope) | spawn sibling **in the parent scope**; re-arm |
| `Message(sid)`, `sid == id` | resume the catch | unchanged | — |
| `EventGateway` | the race is won | unchanged | — |
| `Timer`, `Join`, `Incident`, `Decision` | `Invariant` | `Invariant` (a timer catch hosts nothing; a decision never survives a step) | `Invariant` |

`interrupt_to_boundary` already withdraws the token's remaining arms
(`cancel_attachments`: timers first, then subscriptions, in id order), so a
timer boundary beside the message boundary is cancelled in the same step, and
the trace order is the one the timer boundary already pins:
`message-received b`, `[variables-patched]`, `work-item-cancelled host`,
`[timer-cancelled …]`, `element-started b`, `element-completed b`,
`flow-taken …`.

### 3.4 Withdrawal — nothing new, one invariant restated

Every path that ends a host's wait already withdraws its arms through one
chokepoint:

| Host ends by | Where the arms go |
|---|---|
| completion (`CompleteWorkItem`, `CompleteDecision`, message delivered to the host, scope completed) | `cancel_attachments(token)` — `subscription-cancelled` for the boundary |
| failure caught by an error boundary | `interrupt_to_boundary` → `cancel_attachments` |
| failure uncaught (incident) | `freeze` → `cancel_attachments` |
| interrupting teardown of an enclosing scope | `tear_down_scope` → `withdraw_arms(Some(token))` **with** the token's removal |
| terminate | `withdraw_arms(None)` |

The invariant `TimerTeardown.tla` checks — *no armed row outlives the token it
is armed on* — is the invariant `correlate` depends on too, for the same
reason: its re-check under the instance lock confirms the subscription **row**
survived the unlocked pick, never that the row's **token** did. Today that is
true of subscriptions by the same code path that makes it true of timers
(`withdraw_arms` handles both), but the spec's prose names timers only. §8
re-states it.

### 3.5 Non-interrupting: a sibling token, and re-arming

On a non-interrupting delivery or fire the host's token is **not touched**:
it stays parked, its work item / subscription / scope untouched, and its other
arms untouched. A fresh token `state.next_token_id()` is created and leaves
along the boundary's flow — `element-started b`, `element-completed b`,
`flow-taken f`, then the ordinary queue — in the host token's scope
(`Move { scope: host_token.scope }`). For a subprocess host that is the
*parent* scope, which is where the boundary's flow lives.

**Re-arming.** The boundary must stay active while the host is. For a message
boundary that means a **new subscription** immediately after delivery
(`message-subscribed b <msg> <key>` again, new id, the key **re-evaluated**
against the now-patched document — it is an arm, and arms evaluate at arm
time; the old subscription is already gone, so the duplicate check cannot
trip on itself). For a non-cyclic timer: no re-arm, it fired once. For a
cyclic timer: re-arm if occurrences remain (§3.6).

Emission order: the re-arm is emitted **right after** `message-received` /
`timer-fired` and before the side token moves, so the trace never shows a
live host with its boundary observably absent. Deterministic either way; this
order is the one to pin.

**Scope and instance completion.** A side token is a token in its scope. The
subprocess completes when the scope is empty — including side tokens; the
instance completes when no token remains. So "await payment" completing while
a spawned "add late fee" service task is still open keeps the instance active
until that item completes. That is the spec's semantics and it falls out of
`scope_is_empty` / `run` unchanged. Host completion cancels the *arm*
(`timer-cancelled b`), never the side tokens already spawned. Interrupting
teardown of the scope reaps side tokens with everything else. Terminate takes
all.

### 3.6 Cycles without a clock in the core

The core never interprets time; it must not start now. Division of labour:

- **The core owns the count.** `TimerState` for a cycle carries
  `remaining: Option<u32>` (`None` = unbounded). On `FireTimer` the core emits
  `timer-fired b`, decrements, and if anything remains emits a new
  `TimerArmed { id: new, element, due: Cycle{..}, token, continues:
  Some(old_id) }` — *this arm continues that timer*. The decision to re-arm is
  pure arithmetic on state the core holds, so a replay re-derives it without
  knowing what time it was.
- **The projection owns every instant.** On the first arm of a cycle it
  computes `due_at` from database time: `clock_timestamp() + period` for
  `R/P…`; for an anchored cycle `anchor + ceil((clock_timestamp() − anchor) /
  period) · period` (epoch arithmetic, which is why periods are fixed-length).
  On a re-arm it computes `previous due_at + period` — from the **previous
  due**, not from when the fire actually ran, so a scheduler that was late does
  not drift the schedule. `persist_step` handles events in emission order, so
  `TimerFired` (a `delete … returning due_at`) precedes the `TimerArmed` that
  `continues` it; the returned instant is threaded to the insert inside the
  same loop. No instant is ever stored in the core's state, and none reaches
  the event payload except in the existing `due_at` column.
- **`Display` stays the literal.** `timer-armed late_fee R/P7D` on every
  arm; `remaining` and `continues` are payload, like every reason and every
  answer before them. A golden trace for `R3/P7D` therefore shows three
  `timer-armed` lines and three `timer-fired` lines, which is exactly what
  happened.

What the projection does when an anchored cycle has **no** future occurrence
at arm time (all `n` occurrences are in the past under the phase rule): that
cannot happen — under the phase rule the next occurrence always exists, and
`n` counts from it. This is the second reason for choosing the phase rule over
the fixed-instant one: the alternative would need the projection to tell the
core "nothing to arm" *after* the core had armed it, a round trip the
command-data contract does not have.

### 3.7 Determinism and replay

Nothing enters the core that did not before: a delivery is still command data
with a patch; a fire is still a fact. The storm's `commands_from` already
reconstructs `message-received` → `DeliverMessage` and `timer-fired` →
`FireTimer` by event kind, regardless of the element, so a boundary's history
replays through today's harness unchanged. A cycle's re-arm is a consequence,
not a stimulus, and is re-derived. `WaitKind` gains no variant; persistence
and the `rbpmn_token` CHECK constraint are untouched by slices 1 and 2.

---

## 4. Projection and database

### 4.1 Rows

- An armed boundary subscription is an `rbpmn_subscription` row with
  `element_id` = the boundary's id and `token_no` = the host's token — the
  same shape a timer boundary already has in `rbpmn_timer`. The index
  `rbpmn_subscription_correlate (message_name, correlation_key)` serves it
  unchanged; `correlate_in_tx`'s resolving query is unchanged; the
  "exactly one, or loud" contract is unchanged (`NoSubscription` → 404,
  `AmbiguousCorrelation` → 409).
- **Loader fix (finding 2):** `load_instance` must resolve a `message`-waiting
  token's subscription by `token_no` **and** `element_id = the token's
  element_id`. A token sits at exactly one element and a boundary's id is
  never its host's, so the match is unique by construction — and the fsck
  should say so: *every token with `wait_kind = 'message'` has exactly one
  subscription row with its own element id*. The same reasoning applies to
  `timer` (a timer catch hosts nothing, so there is one row), and making both
  lookups element-qualified is cheaper than remembering why only one needed it.
- Slice 1 needs **no migration**: no new wait kind, no new column. Slice 3
  (cycles) needs **0013**: `rbpmn_timer.due_kind` CHECK gains `'cycle'`, plus
  `remaining int null` and `period interval null` (the parsed fixed-length
  period, so the re-arm's `previous due_at + period` is one expression). All
  additive, metadata-only.

### 4.2 Lock order, and the three-way race

Every path that can end `handle_contest`'s wait takes the **instance row
first** and per-instance rows after, the one order engine-wide
(`spec/LockOrder.tla`):

| Path | Shape | Outcome when it wins | Outcome when it loses |
|---|---|---|---|
| `correlate` (message boundary) | resolve the subscription **without a lock** → `FOR UPDATE` on the instance → re-check the subscription is still in state → step → persist (item → `cancelled`, subscription row deleted) | `200 {instanceId}` | re-check fails → `NoSubscription` (404) — the same answer as a repeat, deliberately |
| `complete_task` | find the item row (no lock) → `FOR UPDATE` on the instance → `guard_lease` reads the item `FOR UPDATE` → step → persist (subscription row deleted by `subscription-cancelled`) | `Advanced` | `guard_lease` sees `cancelled` → `AlreadyClosed { state: "cancelled" }`, before the core is invoked |
| scheduler (a timer boundary on the same host) | try-advisory → `FOR UPDATE NOWAIT` on the instance → re-check the timer row → step | fires | re-check fails → `Attempt::Resolved`, move on |

All three serialize on the instance row; none takes a new lock; the winner's
persist removes the loser's precondition inside the winning transaction. The
cancellation of a *leased* item is an `UPDATE … set state = 'cancelled'` on a
row nobody holds a lock on (a lease is a value), so the holder's open lease
never blocks it and the holder's later statements see the new state. The
claim paths (`get_task`, the push worker) never see a cancelled row: `CLAIMABLE`
requires `available`/lapsed-`locked`.

### 4.3 Competing consumers

Two `correlate` callers for the same `(PAID, T-2026-0042)`: both resolve the
same row, the first to lock delivers, the second's re-check fails → 404. That
is today's contract ("retrying a delivered correlate returns the no-match
error; callers that need blind-retry idempotency make keys unique per
occurrence") and boundaries do not change it. Two replicas' schedulers racing
a delivery: the advisory try-lock spreads them, the row lock decides, the
re-check answers the loser. No `40P01` is possible with one order; the storm
already asserts zero.

---

## 5. The non-interrupting variant, end to end

Most of it is in §3.5–§3.6; this section is the modeller's view and the two
rules the timer case adds.

**Xilium's late fee, as it should read** (fixture sketch in Appendix A):
`await_payment` is a receive task for `PAID`; on it, a non-interrupting timer
boundary `late_fee_due` with `<timeCycle>R/P7D</timeCycle>` leading to a
service task `add_late_fee` and an end event `fee_added`. The arm happens when
`await_payment` is entered; every seven days a side token runs `add_late_fee`
(its handler applies the fee as a patch) and ends; `PAID` completes the host,
`timer-cancelled late_fee_due` withdraws the cycle, and any `add_late_fee`
still open keeps the instance alive until it completes. Anchored form for
"every Monday at midnight Amsterdam time":
`R/2026-08-31T00:00:00+02:00/P7D` — first due the first Monday at or after the
arm, with the DST caveat of §2.5.

**Join semantics.** There are none to add: a side path never reaches a join
(`boundary-side-path`), and joins inside a side path are ordinary blocks.

**Arming table for cycles.**

| Spec | First due | Re-arm | Ends when |
|---|---|---|---|
| `R/P7D` | arm + 7 d | previous due + 7 d | host ends |
| `R3/P7D` | arm + 7 d | previous due + 7 d, while `remaining > 0` | third fire, or host ends |
| `R/2026-08-31T00:00:00+02:00/P7D` | first `anchor + k·7 d ≥ arm` | previous due + 7 d | host ends |
| `R2/2026-08-31T00:00:00+02:00/P7D` | same | same, twice | second fire, or host ends |
| `contract.reminderCycle` (variable) | as the resolved text says | same | same; a bad value is a `timer-resolve-failed` incident at arm time |

**Host completes mid-cycle.** The arm is withdrawn (`timer-cancelled`);
side tokens already spawned are independent and run to their end; the scope
or instance completes when they have. Interrupting the host (another boundary,
an enclosing teardown, terminate) reaps the side tokens in the same scope
together with everything else — they are not special.

**Re-arm and the scheduler.** The re-arm's insert emits `NOTIFY rbpmn_timer`
through the existing `armed_timer` flag in `persist_step`, so a sleeping
scheduler wakes if the next occurrence is sooner than what it slept on — which
for a cycle it never is, but the flag is per step and costs nothing.

---

## 6. History, inspector, editor, DI, HTTP

**Events.** Slice 1 adds **no event kind**: a boundary delivery is
`message-received` on a boundary element id followed by
`work-item-cancelled` / `subscription-cancelled` on the host — new *data*
for a stream consumer (an element id that is a boundary), not a new kind. Slice
2 adds none either. Slice 3 adds payload fields to `timer-armed` (`remaining`,
`continues`) and `TimerDue::Cycle` as a `due` shape; `Display` is unchanged in
format (`timer-armed <element> <literal>`), so no golden trace moves.

**Inspector JSON.** `InstanceInspection.subscriptions` already lists every
open subscription by `elementId`; the element pane for `paid_during_contest`
shows "subscription PAID — key T-2026-0042" with no change, and the
diagnosis line for a correlation incident at a boundary reads correctly
because it looks the incident token's element up in `subscriptions`. Side
tokens appear as additional `tokens` entries at their own elements — the
overlay draws them as it draws any token. `TimerView` gains nothing for slice
1; for cycles, `dueSpec` shows the literal and `dueAt` the next occurrence,
which is what an operator wants to see.

**Editor.** The properties pane already renders `messageRef` for any
`bpmn:MessageEventDefinition` through `renderEventDefinition`, so a message
boundary gets its message-name row for free. Two additions, each with its
slice: an **interrupting** toggle (the standard `cancelActivity` attribute —
nothing vendor-specific) for boundary events in slice 2, and `timeCycle` in
the timer-kind select, offered **only** when the owner is a non-interrupting
boundary, in slice 3 (offering a control the linter refuses "would be
teaching the wrong thing", as the pane's own comment says). The wiring pane
lists correlation rows by element id; the L2 `message-has-correlation`
diagnostic names the boundary's id, so the missing binding shows up in the
same place a catch's does. The editor embeds the linter: **every slice owes
`just ui`.**

**DI.** bpmn-js renders `cancelActivity="false"` as the dashed double circle
— standard notation, no work. New fixtures get their DI from
`just fixtures-di`; a boundary's shape sits on its host's border and the
generated layout handles that already (`09-timer-boundary.bpmn`).

**HTTP.** `POST /v1/messages {name, correlationKey, patch}` is **unchanged**
in request, response and status codes. The task API is unchanged; what
changes is that `{"outcome":"alreadyClosed","state":"cancelled"}` becomes an
outcome a user-task frontend should expect on an ordinary day, and the
application docs should say so (and what `lockLost` means in that light).

---

## 7. Migration and version pinning

- **Existing deployed definitions** were linted under the stricter rules, so
  none contains a message boundary, a non-interrupting boundary or a cycle. No
  stored definition can compile into a new `ExecKind`; the compile cache is
  keyed by definition id; instances pin their version for life. Relaxing the
  three rules cannot change any stored verdict.
- **Live instances** are unaffected: no token wait kind changes, no row shape
  changes, `rbpmn_token`'s CHECK is untouched. An instance parked at a user
  task today rehydrates identically tomorrow.
- **The one retroactively stricter rule** is finding 3: a stored definition
  with a timer boundary on a business-rule task would fail
  `check_active_definitions` at the next boot — precisely the "upgrade escape
  hatch" row in the open-items table ("retroactively-stricter lint can refuse
  to boot with the deploy API unreachable"). Recommendation: ship it as the
  error it is, and make the upgrade note say: *before upgrading, lint your
  deployed definitions with the new build (`POST /v1/definitions/lint`, or
  the editor) — a timer boundary on a business-rule task has never fired and
  must be removed.* With one application in the field and that shape known to
  be absent from it, this is the cheap moment to take the first stricter
  rule; the escape hatch stays queued for the upgrade that cannot be
  pre-checked.
- `just parity` covers the new fixtures the moment they enter the corpus;
  both WASM exports are compared against native, so the editor cannot report
  a different verdict than deploy. `just ui` is owed by every slice.

---

## 8. TLA+

Touching the lease and the correlate path means **re-reading `spec/`, not
only re-running `just tla`** (the DMN round's lesson, in CLAUDE.md). What each
model has to say:

**`LockOrder.tla` — re-read, no change.** `correlate` keeps its shape (pick
without a lock, instance row, per-instance rows); cancelling a leased item is a
write to a row already covered by the instance lock. No new lock enters the
order, at either arity. Record that in `spec/README.md`'s inventory table as a
re-audit, the way the DMN note did.

**`TimerTeardown.tla` — re-state, no new state.** The protocol it models —
an unlocked pick of an arm row, a teardown that may commit in the window, a
re-check under the lock that sees the *row* and not the *token* — is exactly
`correlate`'s, with `rbpmn_subscription` for `rbpmn_timer`. The module is
already symbolic over "arm rows"; rename the prose to say so and add a
`SubscriptionTeardown.cfg` that is the same config under the other name, so
the README row reads "no armed row — timer **or subscription** — outlives its
token". Not decoration: it is the invariant `correlate`'s re-check leans on
once a boundary subscription can be armed on a token that teardown reaps.

**`Lease.tla` — add `Cancel` (finding 4).** A new action: the process
withdraws the item — interrupting boundary, terminate, scope teardown. Guard:
`state ∈ {available, locked}` (no liveness clause: the process does not care
about the lease); effect: `state' = "cancelled"`, owner and deadline cleared,
`lastActor' = Process` (a new model value beside `NoOne`). `Complete` on a
cancelled item is the `AlreadyClosed` no-op, like `done`. Then:

- `LiveLeaseEndsOnlyByItsHolder` must become
  `LiveLeaseEndsOnlyByItsHolderOrTheProcess` — the honest property, and the
  first time the model states that a live lease does not protect against the
  process. Left as is, the property would *fail* on the shipped code the
  moment `Cancel` is added, which is the correct way to discover it was never
  true.
- New: `CancelledIsNeverCompleted == state = "cancelled" => completions = 0`
  and, as an action property, *after `Cancel` every `Complete(w)` is the
  no-op* — the brief's "the lease holder's later `complete_task` must come
  back `AlreadyClosed`, never succeed".
- Counterexample config `Lease_CancelIgnoresGuard.cfg`: let `Complete` skip the
  closed-item check (`Completable` without `state # "done" /\ state #
  "cancelled"`) and TLC must find a completion after a cancel. That is the
  config that proves the property has teeth.

**New: `BoundaryExit.tla` — the exactly-once property the brief asks for.**
One token parked at a host work item with one armed boundary subscription.
Nodes are symmetric: any node may run `complete_task` or `correlate`. The
model (sketch in Appendix C) has the shape of `TimerTeardown` — `Pick` without
a lock, then a locked action with a re-check — for the delivery side, and a
locked `Complete` for the other. Properties:

- `ExactlyOneExit == completions + deliveries ≤ 1`, and in every terminal
  state `= 1` (checked as an invariant plus a deadlock-free run: the only
  terminal states are the two exits).
- `ArmDiesWithTheWait == armed ⇒ state = "open"` — completion withdraws the
  subscription in its own transaction; the TimerTeardown invariant, on this
  path.
- `LateCallsAreTyped` — after an exit, `Complete` answers `AlreadyClosed` and
  `Deliver` answers `NoSubscription`; neither reaches `step`. In the model:
  no action ever takes the "step" branch with its precondition false.
- Two expected-fail configs: `BoundaryExit_NoRecheck.cfg` (deliver on the
  unlocked pick without re-checking under the lock → a delivery lands on a
  completed host → two exits) and `BoundaryExit_NoWithdraw.cfg` (completion
  leaves the subscription row → a late `PAID` interrupts a task that already
  completed → two exits). Each matched against its specific violation in the
  `just tla` table, never by exit code alone.

Bounded: 2 nodes, 1 token, 1 work item, 1 subscription, plus a second
subscription on another token to show the re-check cannot be satisfied by "a
row exists" rather than "*this* row exists". Enough for the interleavings
that matter; exhaustive only within those bounds.

---

## 9. Test plan

**Fixtures first**, as every phase. Names follow the corpus' numbering.

Accept (`crates/rbpmn-model/tests/fixtures/accept/`):

| Fixture | Slice | What it shows |
|---|---|---|
| `29-message-boundary.bpmn` | 1 | the Xilium shape: user task, interrupting `PAID` boundary (Appendix A) |
| `30-receive-task-message-boundary.bpmn` | 1 | receive task for `PAID` with an interrupting `CANCELLED` boundary — two subscriptions on one token |
| `31-subprocess-message-boundary.bpmn` | 1 | a subprocess with work inside, interrupted by a message — teardown through the message path |
| `32-message-and-timer-boundaries.bpmn` | 1 | both kinds on one host; either interrupts, the other is withdrawn |
| `33-non-interrupting-message-boundary.bpmn` | 2 | side path to its own end; the host continues; re-arm |
| `34-late-fee-cycle.bpmn` | 3 | receive task with `R/P7D` non-interrupting timer → service task → end |
| `35-anchored-cycle.bpmn` | 3 | `R2/2026-08-31T00:00:00+02:00/P7D` |
| `36-cycle-from-variable.bpmn` | 3 | `timeCycle` naming a variable; `warn timer-expression` |

Reject:

| Fixture | Expected | Slice |
|---|---|---|
| `message-boundary-no-ref.bpmn` | `error message-has-correlation @ b` | 1 |
| `boundary-on-business-rule-task.bpmn` | `error boundary-on-supported-host @ b` (timer) — **moves today's accepted dead arm to a rejection** | 1 |
| `side-path-merges-back.bpmn` | `error boundary-side-path @ b` — the reminder path rejoining the host's continuation | 2 |
| `side-path-into-join.bpmn` | `error boundary-side-path @ b` — and a mutation-test entry showing the `second token arrived at join` invariant without the rule | 2 |
| `cycle-on-interrupting-boundary.bpmn` | `error no-unsupported-element @ b` | 3 |
| `cycle-on-catch.bpmn` | `error no-unsupported-element @ c` — today's `timer-cycle.bpmn`, message rewritten | 3 |
| `cycle-zero-repeats.bpmn`, `cycle-month-period.bpmn`, `cycle-with-end.bpmn` | `error timer-iso8601 @ b` | 3 |

Today's `reject/non-interrupting-boundary.bpmn` moves to accept in slice 2 and
`reject/timer-cycle.bpmn` becomes `cycle-on-catch.bpmn` in slice 3 — each in
the change that makes it executable, not before.

L2 (`check_deployable`, in `rbpmn-core/src/check.rs` tests and the editor's
corpus): `ambiguous-message-arm` for the three certain shapes, and the
*negative* — same message, different binding names — accepted.

**Scenarios** (`crates/rbpmn-core/tests/scenarios/`, golden traces; the two
for the Xilium fixture are written out in Appendix B):

`29-message-boundary-delivered.json`, `29-message-boundary-completed.json`,
`30-receive-host-delivered.json`, `30-receive-boundary-delivered.json`,
`31-subprocess-message-boundary.json`, `32-message-wins.json`,
`32-timer-wins.json`, `33-non-interrupting-twice-then-complete.json`,
`34-cycle-fires-twice-then-paid.json`, `34-paid-while-fee-task-open.json`
(the instance stays active until the side token ends),
`35-anchored-cycle-exhausts.json`. The scenario runner's action vocabulary
(`deliver`, `fire`, `complete`) already addresses arms by element id, so
`{"deliver": "paid_during_contest"}` works without runner changes.

**Core property/explorer tests.** The explorer enumerates `DeliverMessage`
for every open subscription in the state and picks scenarios up from the
corpus — boundary subscriptions are explored for free. Its
`reachable_conditions` walk must learn `boundaries(n)` beside
`timer_boundaries(n)` (it hard-codes the latter). Property test to add:
*for any interleaving of `CompleteWorkItem` and `DeliverMessage` on one host,
exactly one succeeds, the other returns `WorkItemNotOpen` /
`UnknownSubscription` before mutating, and the final trace is one of the two
golden ones* — the core-level statement of `BoundaryExit.tla`.

**Model generator (tier 1).** Add `MsgBoundary(Box<Block>)` — a task carrying
an interrupting message boundary whose path is a block ending at an end
event — and a driver choice between completing the task and delivering the
message; the oracle counts whichever path was taken. A green storm after
slice 1 without this would be "nothing already covered broke", not coverage
(`docs/stress-testing.md` §3-bis).

**Engine tests** (`crates/rbpmn-engine/tests/engine.rs`), beside the boundary
timer and correlate tests already there:

- `message_boundary_interrupts_a_leased_user_task`: claim → correlate →
  `complete_task` returns `AlreadyClosed{cancelled}`; `extend_lock` → `Lost`;
  `release_task` → `Lost`; the patch from the refused completion is absent
  from the document.
- `completion_wins_then_the_message_is_404`.
- `correlate_and_complete_race_exactly_one_wins`: N rounds, both verbs
  launched concurrently on fresh instances, assert exactly one `Ok` per round
  and that the refusal is the typed one — and non-vacuity: both orders
  occurred across the rounds (the storm's "the race went both ways" pattern).
- `message_boundary_on_a_receive_task_rehydrates_the_right_subscription`:
  park, evict the compile cache or reconnect, deliver to the *host* and to the
  *boundary* in separate runs — the loader fix under test.
- `message_boundary_tears_down_a_subprocess_scope` (mirror of the timer one).
- `competing_correlators_deliver_once` (two callers, same key).
- Slice 3: `cycle_rearms_from_the_previous_due_not_from_fire_time` (fire late,
  assert the next `due_at` is `previous + period`), `anchored_cycle_first_due_
  is_the_next_phase`, `host_completion_cancels_the_cycle_and_keeps_side_work`.

**fsck** (`tests/harness/mod.rs`): *every `message`-waiting token has exactly
one subscription row at its own element*; *every subscription row's token
exists* (the `ArmedSubscriptionsHaveLiveTokens` invariant, as SQL); for
cycles, *every `cycle` timer row has a non-null `period`*.

**Storm and chaos.** Add `29-message-boundary.bpmn` to both workloads with a
driver that completes or correlates at random, and assert non-vacuity the way
the boundary-timer race already does ("never went both ways" fails the run).
Replay verification needs no change — `message-received` on a boundary element
reconstructs to `DeliverMessage` already.

**Untouched, deliberately:** `just feel-parity` — the condition grammar does
not change and correlation names reuse `parse_qname`; `just number-parity`
and `just dmn-tck` — no dsntk involvement. **Owed** by every slice: `cargo
test`, `just lint`, `just parity`, `just ui`, `just tla`; by slice 3
additionally `just bench-population` once (a million armed cycle timers is
the population the scheduler's sleep query was tuned on, and `period` is a
new column on the row it walks).

---

## 10. Staging and effort

Estimates are working days for one developer who has read this document,
including fixtures, tests, spec changes and docs (README catalogue, the
open-items table, `spec/README.md`). Each slice is independently shippable
and each ends with the owed commands green.

### Slice 1 — interrupting message boundary (≈ 3–4 days)

The brief names *user + receive tasks* as the smallest slice. The
recommendation is **all four hosts** — user, service, receive task,
subprocess — and the reason is that the smaller slice is *more* code, not
less: the core arms boundaries through one `arm_boundaries` and interrupts
through one `interrupt_to_boundary`, so restricting hosts means adding a
`NotYetExecutable` arm and a lint message to refuse a service task or a
subprocess, and then removing both later. The subprocess host costs one
fixture, one scenario and one engine test, on a teardown path that is already
model-checked. The service task costs nothing at all beyond its fixture —
`WaitKind::WorkItem` is `WaitKind::WorkItem`.

- lint: the relaxation, `boundary-on-supported-host` for the business-rule
  task, `ambiguous-message-arm` at L2; fixtures 29–32 and the rejects — 0.5 d
- core: `MessageBoundary`, `boundaries(host)`, the `DeliverMessage` arms,
  `message_arm` accessor; scenarios — 0.5 d
- projection: the loader fix, fsck rows; engine tests incl. the race — 1 d
- spec: `Lease.tla` `Cancel` + property rename + counterexample config,
  `BoundaryExit.tla` + two configs, `TimerTeardown` prose and second config,
  README rows — 1 d
- editor (`just ui`), README catalogue, open-items row, model-generator
  `MsgBoundary`, storm/chaos workload — 0.5–1 d

### Slice 2 — non-interrupting message boundary (≈ 3 days)

- lint: `boundary-side-path`, the `Graph` split into interrupting /
  non-interrupting pseudo-edges, `regions.rs` walking only the former;
  fixture 33 and the two side-path rejects; the mutation-test entry — 1 d
- core: `interrupting: false` on both boundary kinds, sibling spawn, message
  re-arm, emission order; scenarios — 0.5 d
- engine tests, storm driver variant — 0.5 d
- editor: the `cancelActivity` toggle; `just ui`; docs — 0.5 d
- spec: re-read only — a side token is an ordinary token; `LockOrder` and
  `BoundaryExit` are unaffected, and the README should say that was checked —
  0.5 d

### Slice 3 — non-interrupting timer boundary + `timeCycle` (≈ 4–5 days)

- `iso8601::validate_cycle` and the cycle grammar rules; fixtures 34–36 and
  the cycle rejects; `timer-expression` for `TimerKind::Cycle` — 1 d
- core: `TimerDue::Cycle`, `remaining`, `continues`, the re-arm; scenarios —
  0.5 d
- projection: migration 0013, the first-due and re-arm arithmetic in
  `persist_step` (threading `returning due_at`), rehydration of `remaining` —
  1 d
- scheduler/engine tests (late fire does not drift; anchored phase; host
  completion mid-cycle), `bench-population` sanity — 1 d
- editor: `timeCycle` for non-interrupting boundaries only; `just ui`; docs,
  including the phase-rule and DST statements in README — 0.5 d

### Deferred, with the reason

| Item | Why not now |
|---|---|
| `LockExtension::Lost { state }` / `Released::Lost { state }` | additive; a UX improvement, not a correctness one; ships when a frontend asks |
| static detection of duplicate arms on parallel branches | needs the region analysis to reason about concurrency; the runtime freeze is loud meanwhile |
| calendar-aware cycles (`P1M`, DST-stable local time) | a different arithmetic and a timezone database; fixed-length periods cover the motivating case |
| cycles on intermediate catches / interrupting boundaries | rejected on purpose — "fire once and ignore the rest" is the silent behaviour this engine refuses |
| event subprocesses | v3 in the roadmap; reuse this round's arming and side-token machinery when they come |

---

## Appendix A — fixture sketches

Ready for `just fixtures-di` (no DI here; it adds it). Each carries its
`expect-diagnostics` comment in the corpus' form.

### `accept/29-message-boundary.bpmn` — the Xilium shape

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!-- expect-diagnostics:
-->
<!-- A user task interrupted by a message: a payment arriving while a ticket
     is being contested ends the ticket as Paid. The clerk's pending
     completion then comes back AlreadyClosed{cancelled}, never succeeds.
     Bindings: {"correlations": {"paid_during_contest": "ticket.reference"}} -->
<bpmn:definitions xmlns:bpmn="http://www.omg.org/spec/BPMN/20100524/MODEL"
    id="defs" targetNamespace="https://rbpmn.dev/fixtures">
  <bpmn:message id="m_paid" name="PAID" />
  <bpmn:process id="ticket" isExecutable="true">
    <bpmn:startEvent id="start">
      <bpmn:outgoing>f1</bpmn:outgoing>
    </bpmn:startEvent>
    <bpmn:userTask id="handle_contest" name="Handle contest">
      <bpmn:incoming>f1</bpmn:incoming>
      <bpmn:outgoing>f2</bpmn:outgoing>
    </bpmn:userTask>
    <bpmn:boundaryEvent id="paid_during_contest" attachedToRef="handle_contest">
      <bpmn:outgoing>f3</bpmn:outgoing>
      <bpmn:messageEventDefinition messageRef="m_paid" />
    </bpmn:boundaryEvent>
    <bpmn:endEvent id="end_decided" name="Contest decided">
      <bpmn:incoming>f2</bpmn:incoming>
    </bpmn:endEvent>
    <bpmn:endEvent id="end_paid" name="Paid">
      <bpmn:incoming>f3</bpmn:incoming>
    </bpmn:endEvent>
    <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="handle_contest" />
    <bpmn:sequenceFlow id="f2" sourceRef="handle_contest" targetRef="end_decided" />
    <bpmn:sequenceFlow id="f3" sourceRef="paid_during_contest" targetRef="end_paid" />
  </bpmn:process>
</bpmn:definitions>
```

### `accept/34-late-fee-cycle.bpmn` — slice 3

```xml
<!-- expect-diagnostics:
-->
<!-- "Every 7 days while waiting for payment, add a late fee." The cycle is
     armed when await_payment is entered; each occurrence spawns a side token
     through add_late_fee; PAID completes the host and cancels the cycle. A
     fee task still open keeps the instance alive until it completes.
     Bindings: {"correlations": {"await_payment": "ticket.reference"},
                "topics": {"add_late_fee": "fees"}} -->
<bpmn:message id="m_paid" name="PAID" />
<bpmn:process id="ticket" isExecutable="true">
  <bpmn:startEvent id="start"><bpmn:outgoing>f1</bpmn:outgoing></bpmn:startEvent>
  <bpmn:receiveTask id="await_payment" name="Await payment" messageRef="m_paid">
    <bpmn:incoming>f1</bpmn:incoming><bpmn:outgoing>f2</bpmn:outgoing>
  </bpmn:receiveTask>
  <bpmn:boundaryEvent id="late_fee_due" cancelActivity="false" attachedToRef="await_payment">
    <bpmn:outgoing>f3</bpmn:outgoing>
    <bpmn:timerEventDefinition>
      <bpmn:timeCycle>R/P7D</bpmn:timeCycle>
    </bpmn:timerEventDefinition>
  </bpmn:boundaryEvent>
  <bpmn:serviceTask id="add_late_fee" name="Add late fee">
    <bpmn:incoming>f3</bpmn:incoming><bpmn:outgoing>f4</bpmn:outgoing>
  </bpmn:serviceTask>
  <bpmn:endEvent id="end_paid" name="Paid"><bpmn:incoming>f2</bpmn:incoming></bpmn:endEvent>
  <bpmn:endEvent id="fee_added" name="Fee added"><bpmn:incoming>f4</bpmn:incoming></bpmn:endEvent>
  <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="await_payment" />
  <bpmn:sequenceFlow id="f2" sourceRef="await_payment" targetRef="end_paid" />
  <bpmn:sequenceFlow id="f3" sourceRef="late_fee_due" targetRef="add_late_fee" />
  <bpmn:sequenceFlow id="f4" sourceRef="add_late_fee" targetRef="fee_added" />
</bpmn:process>
```

### `reject/side-path-merges-back.bpmn` — slice 2

```xml
<!-- expect-diagnostics:
  error boundary-side-path @ remind
-->
<!-- A non-interrupting reminder that rejoins the flow after the task: the
     rest of the process would run once per reminder plus once for the
     approval. Use an interrupting boundary and a loop instead. -->
<bpmn:process id="p" isExecutable="true">
  <bpmn:startEvent id="start"><bpmn:outgoing>f1</bpmn:outgoing></bpmn:startEvent>
  <bpmn:userTask id="approve"><bpmn:incoming>f1</bpmn:incoming><bpmn:outgoing>f2</bpmn:outgoing></bpmn:userTask>
  <bpmn:boundaryEvent id="remind" cancelActivity="false" attachedToRef="approve">
    <bpmn:outgoing>f3</bpmn:outgoing>
    <bpmn:timerEventDefinition><bpmn:timeDuration>P2D</bpmn:timeDuration></bpmn:timerEventDefinition>
  </bpmn:boundaryEvent>
  <bpmn:serviceTask id="notify"><bpmn:incoming>f3</bpmn:incoming><bpmn:outgoing>f4</bpmn:outgoing></bpmn:serviceTask>
  <bpmn:serviceTask id="archive"><bpmn:incoming>f2</bpmn:incoming><bpmn:incoming>f4</bpmn:incoming><bpmn:outgoing>f5</bpmn:outgoing></bpmn:serviceTask>
  <bpmn:endEvent id="end"><bpmn:incoming>f5</bpmn:incoming></bpmn:endEvent>
  <bpmn:sequenceFlow id="f1" sourceRef="start" targetRef="approve" />
  <bpmn:sequenceFlow id="f2" sourceRef="approve" targetRef="archive" />
  <bpmn:sequenceFlow id="f3" sourceRef="remind" targetRef="notify" />
  <bpmn:sequenceFlow id="f4" sourceRef="notify" targetRef="archive" />
  <bpmn:sequenceFlow id="f5" sourceRef="archive" targetRef="end" />
</bpmn:process>
```

`side-path-into-join.bpmn` is the same idea inside a parallel region, with
`f4` entering the region's join; it doubles as the mutation-test
counterexample (`second token arrived at join 'pj' via flow 'f4'`).

### `reject/boundary-on-business-rule-task.bpmn` — slice 1, the dead arm

```xml
<!-- expect-diagnostics:
  error boundary-on-supported-host @ bt
-->
<!-- Accepted today, and it has never fired: the decision is answered inside
     the transaction that parks the token, so the timer is armed and
     cancelled in one step. -->
<bpmn:businessRuleTask id="decide">…</bpmn:businessRuleTask>
<bpmn:boundaryEvent id="bt" attachedToRef="decide">
  <bpmn:outgoing>f3</bpmn:outgoing>
  <bpmn:timerEventDefinition><bpmn:timeDuration>PT1H</bpmn:timeDuration></bpmn:timerEventDefinition>
</bpmn:boundaryEvent>
```

## Appendix B — golden traces for fixture 29

`29-message-boundary-delivered.json`:

```json
{
  "fixture": "accept/29-message-boundary.bpmn",
  "bindings": { "correlations": { "paid_during_contest": "ticket.reference" } },
  "variables": { "ticket": { "reference": "T-2026-0042" } },
  "actions": [{ "deliver": "paid_during_contest", "patch": { "payment": { "amount": 60 } } }],
  "expect": {
    "status": "completed",
    "variables": { "ticket": { "reference": "T-2026-0042" }, "payment": { "amount": 60 } },
    "trace": [
      "instance-started",
      "element-started start",
      "element-completed start",
      "flow-taken f1",
      "element-started handle_contest",
      "work-item-created handle_contest user handle_contest",
      "message-subscribed paid_during_contest PAID T-2026-0042",
      "message-received paid_during_contest PAID",
      "variables-patched",
      "work-item-cancelled handle_contest",
      "element-started paid_during_contest",
      "element-completed paid_during_contest",
      "flow-taken f3",
      "element-started end_paid",
      "element-completed end_paid",
      "instance-completed"
    ]
  }
}
```

`29-message-boundary-completed.json` — the clerk wins:

```json
{
  "fixture": "accept/29-message-boundary.bpmn",
  "bindings": { "correlations": { "paid_during_contest": "ticket.reference" } },
  "variables": { "ticket": { "reference": "T-2026-0042" } },
  "actions": [{ "complete": "handle_contest", "patch": { "contest": { "upheld": true } } }],
  "expect": {
    "status": "completed",
    "variables": { "ticket": { "reference": "T-2026-0042" }, "contest": { "upheld": true } },
    "trace": [
      "instance-started",
      "element-started start",
      "element-completed start",
      "flow-taken f1",
      "element-started handle_contest",
      "work-item-created handle_contest user handle_contest",
      "message-subscribed paid_during_contest PAID T-2026-0042",
      "work-item-completed handle_contest",
      "variables-patched",
      "subscription-cancelled paid_during_contest PAID",
      "element-completed handle_contest",
      "flow-taken f2",
      "element-started end_decided",
      "element-completed end_decided",
      "instance-completed"
    ]
  }
}
```

Both orders are the existing orders: the delivery trace is `FireTimer`'s
`WorkItem` arm with `message-received` for `timer-fired`; the completion
trace is `CompleteWorkItem`'s, with the boundary's `subscription-cancelled`
where a timer boundary's `timer-cancelled` sits today
(`09-timer-boundary-completed.json`).

## Appendix C — `BoundaryExit.tla`, a sketch

```tla
------------------------------ MODULE BoundaryExit ------------------------------
(* One token parked at a host work item with one armed boundary              *)
(* subscription. Any node may run complete_task or correlate. Both lock the  *)
(* instance row; correlate picks its subscription WITHOUT a lock first and   *)
(* re-checks under it — the TimerTeardown shape. Exactly one exit may ever   *)
(* happen, and a late call of either verb is answered typed, never stepped.  *)
EXTENDS Naturals

CONSTANTS Nodes, NoPick, Recheck, WithdrawOnComplete

VARIABLES
    item,         \* "open" | "completed" | "cancelled"   (rbpmn_work_item.state)
    armed,        \* TRUE while the boundary's subscription row exists
    picked,       \* Nodes -> BOOLEAN: resolved the subscription without a lock
    completions,  \* how many times the host completed
    deliveries,   \* how many times the boundary was taken
    stepped       \* TRUE if step() was ever invoked with its precondition false

vars == <<item, armed, picked, completions, deliveries, stepped>>

Init ==
    /\ item = "open" /\ armed = TRUE
    /\ picked = [n \in Nodes |-> FALSE]
    /\ completions = 0 /\ deliveries = 0 /\ stepped = FALSE

\* complete_task: instance row, guard_lease reads the item, then step.
\* Completion withdraws the boundary's arm in the same transaction
\* (cancel_attachments) — unless the buggy config says otherwise.
Complete(n) ==
    /\ item = "open"
    /\ item' = "completed"
    /\ armed' = IF WithdrawOnComplete THEN FALSE ELSE armed
    /\ completions' = completions + 1
    /\ UNCHANGED <<picked, deliveries, stepped>>

\* ...and AlreadyClosed: the typed no-op, before the core is invoked.
CompleteLate(n) ==
    /\ item # "open"
    /\ UNCHANGED vars

\* correlate, first half: the unlocked resolve.
Pick(n) ==
    /\ ~picked[n] /\ armed
    /\ picked' = [picked EXCEPT ![n] = TRUE]
    /\ UNCHANGED <<item, armed, completions, deliveries, stepped>>

\* correlate, second half: instance row, re-check, step. The interrupting
\* delivery cancels the item and consumes the subscription row.
Deliver(n) ==
    /\ picked[n]
    /\ Recheck => armed
    /\ stepped' = stepped \/ ~armed \/ item # "open"
    /\ item' = IF item = "open" THEN "cancelled" ELSE item
    /\ armed' = FALSE
    /\ deliveries' = deliveries + 1
    /\ picked' = [picked EXCEPT ![n] = FALSE]
    /\ UNCHANGED completions

\* ...and NoSubscription: the re-check lost.
DeliverLate(n) ==
    /\ picked[n] /\ ~armed
    /\ picked' = [picked EXCEPT ![n] = FALSE]
    /\ UNCHANGED <<item, armed, completions, deliveries, stepped>>

Next == \E n \in Nodes :
    Complete(n) \/ CompleteLate(n) \/ Pick(n) \/ Deliver(n) \/ DeliverLate(n)

Spec == Init /\ [][Next]_vars

ExactlyOneExit    == completions + deliveries <= 1
ArmDiesWithTheWait == armed => item = "open"
LateCallsAreTyped == stepped = FALSE
================================================================================
```

Configs: `BoundaryExit.cfg` (`Recheck = TRUE`, `WithdrawOnComplete = TRUE`,
all three invariants hold); `BoundaryExit_NoRecheck.cfg` (`Recheck = FALSE`
→ `ExactlyOneExit` and `LateCallsAreTyped` fail: a pick made before a
completion is delivered after it); `BoundaryExit_NoWithdraw.cfg`
(`WithdrawOnComplete = FALSE` → `ArmDiesWithTheWait` fails first, then
`ExactlyOneExit`: a `PAID` after the contest was decided interrupts a task
that no longer exists). Each expected failure is matched against its own
invariant name in the `just tla` table, so a spec that stops parsing cannot
read as "fails as expected".
