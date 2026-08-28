# The published read surface

Six views on the same footing as rule ids and the `Display` format of
`Event`: columns may be added, never removed or repurposed.

Applications legitimately need to *join* rbpmn's state against their own rows.
A result set of "our tenancy, our ordering, rbpmn's instances" is a SQL join,
and no API returning data instead of SQL does it as well. The answer to "stop
reading my schema" is not to stop reading; it is to publish the surface. So
rbpmn publishes six views as **public API**, on the same footing as rule ids
and the `Display` format of `Event`, one per thing an instance can be doing:

| view | the question it answers |
|---|---|
| `rbpmn_v_definition` | what is deployed, at which version, and from which artifacts |
| `rbpmn_v_definition_decision` | the DMN artifacts a version was deployed with |
| `rbpmn_v_instance` | what is running, and what does it hold |
| `rbpmn_v_work_item` | what is waiting to be worked, and how deep is each queue |
| `rbpmn_v_timer` | when does this next happen |
| `rbpmn_v_subscription` | what is waiting on this business identifier |

The last three are the three things an instance can be *waiting* on — a
worker, a clock, a message — so between them there is no wait state an
application has to read an undocumented table to see. The first two are what
is deployed rather than what is happening: the question behind them is
reconciliation, "is the model running here the one in git?", which is
`content_hash` against a hash of the bundle.

They compose on `instance_id`, so a statement can group deadlines and queue
depths by an application's own dimensions at once. The constants `rbpmn_engine::{DEFINITION_VIEW, DEFINITION_DECISION_VIEW,
INSTANCE_VIEW, WORK_ITEM_VIEW, TIMER_VIEW, SUBSCRIPTION_VIEW}` name them, so a rename is a compile error for
callers rather than a runtime surprise.

**One contract, for all six.** Columns may be added; none will be removed or
repurposed. Each is deliberately a **plain inlinable projection** — no `WHERE`,
no `LIMIT`, no `DISTINCT`, no `ORDER BY`, no aggregate, no volatile function
(`now()` is STABLE, which is what makes time-dependent columns legal), and
explicitly **not** `security_barrier`. A barrier view refuses to push an
outside predicate below itself unless the operators are leakproof, and
`jsonb ->>` is not one, so every declared variable index would sit unused
beneath a full scan. Each has an EXPLAIN-based test asserting the *plan*
through the view, not just its shape.

**None of them is a tenancy boundary.** They do no row filtering and rbpmn
manages no grants: what a connection can see, it can see. An application that
needs a boundary expresses it in its own query, which is the whole reason this
surface is SQL.

**And none of them is a claim.** They are read models: a value is true when it
was measured. A depth of five does not reserve five items, an armed timer is
not a promise about when it fires, and the only way to *hold* work is
`get_task`.

## `rbpmn_v_definition` and `rbpmn_v_definition_decision`

| column | |
|---|---|
| `id` | the definition id `deploy` returns |
| `key`, `version` | the stable pair everything else joins on |
| `content_hash` | sha256 of the bundle — deploy's own idempotency key |
| `deployed_at` | when this version landed |
| `bpmn_xml`, `bindings` | the artifacts that are 1:1 with a definition |
| `retired_instances` | why `delete_definition` may refuse a version that looks unreferenced |

The DMN artifacts are 0..N, so folding them in would need an `array_agg` and
that would stop the view being an inlinable projection — the same reason
`rbpmn_v_subscription` leaves ambiguity to a query. They get
`rbpmn_v_definition_decision` (`definition_id`, `definition_key`,
`definition_version`, `ordinal`, `dmn_xml`), joinable either way. **Read them
ordered by `ordinal`:** artifacts may import one another, so deployment order
is part of the deployment.

⚠️ `bpmn_xml` and `dmn_xml` are whole documents. `select *` here pulls every
model in the installation across the wire — name your columns, and reach for
the XML only when the answer *is* the model. The deployment inventory asked
for most often is `Engine::DEPLOYED_NOW_SQL`, which selects no XML at all:

```sql
select distinct on (key) key, version, content_hash, deployed_at
  from rbpmn_v_definition order by key, version desc
```

No index was added for these. Definitions are bounded by deploys rather than
by throughput — a few versions per process — so there is no scan worth
preventing, and the primary key plus the `(key, version)` unique index already
serve both "this exact version" and "the latest of this key".

## `rbpmn_v_instance`

| column | |
|---|---|
| `id` | instance id |
| `definition_key`, `definition_version` | the stable coordinates; instances pin a version |
| `business_key` | as passed to `start` — nullable, non-unique, unindexed |
| `status` | `active` / `completed` / `terminated` / `failed` |
| `variables` | the whole live variable document |
| `created_at`, `completed_at` | |

## `rbpmn_v_work_item`

The question is a triage screen's first paint: *for every queue this user can
work, how many items are waiting right now?* `count_tasks(topic, filter)`
answers it one queue at a time, so a dashboard covering T topics across D
definitions cost T×D round trips. Here it is one statement.

| column | |
|---|---|
| `id`, `instance_id`, `item_no` | identity; `(instance_id, item_no)` is the stable pair |
| `definition_key`, `definition_version`, `element_id` | where in which model |
| `topic`, `kind` | the queue, and `user` / `service` |
| `state` | `available` / `locked` / `completed` / `cancelled` / `failed` |
| **`claimable`** | **would `get_task` hand this out right now** |
| **`in_progress`** | held under a lease that has not expired |
| `lock_owner`, `lock_until` | who holds it, until when |
| `retry_at`, `retries`, `failures`, `last_failure` | why it is stuck |
| `created_at` | |

**`claimable` is computed by the engine**, and that is the point of the
column. It is not `state = 'available'`: it accounts for a lapsed lease
(claimable again), a live lease (not), retry backoff not yet due (not), closed
states (never), and an instance frozen on an incident (never — which is why
the view joins instances). A dashboard whose depths disagree with what
`get_task` hands out is worse than no dashboard, so it is the same expression
the claim path uses, and a test differentials the two row for row.

`in_progress` is about the **lease alone**, so `waiting + in_progress` is not
"every open item" — work belonging to a frozen instance is in neither bucket.
That gap is information, not an omission.

## `rbpmn_v_timer`

The question is *when does this next happen?* — a renewal date, a payment
reminder, an escalation that has not fired yet.

| column | |
|---|---|
| `instance_id`, `timer_no` | identity |
| `definition_key`, `definition_version`, `element_id` | where in which model |
| `due_at` | the instant it is armed for |
| `due_kind` | `duration` / `date` / `cycle` |
| **`due_spec`** | **the literal or variable path the arm resolved from** |
| `remaining` | fires left on a cycle, including this one; null when unbounded and on non-cycles |
| `instance_status` | so "scheduler behind" and "instance frozen" are one query apart |
| `created_at` | when it was armed |

`due_spec` is the load-bearing column. An operator asking *why is it due
then* needs the source of the instant, not the instant — and for a cycle it
carries the period too, inside the repetition (`R/P7D`).

**A row is what is armed, not a promise about when it fires.** A `due_at` in
the past means "due and not yet fired", not "late": the scheduler runs on its
own cadence and fires at most one timer per pass. Whether it takes this one
next also depends on the instance being active (hence `instance_status`) and
on a node's transient, in-process deferral set, which no view can see.

**For a cycle the row is the next occurrence, never the series.** Firing
deletes the row and inserts the next in the same transaction, so there is
exactly one row per armed cycle and an application rendering a date never has
to guess which one it has.

There is deliberately **no `overdue` boolean.** It would be legal, but unlike
`claimable` it encodes no rule — it is `due_at < now()` and nothing else.
Compare `due_at` directly, which also gets you the range queries a boolean
cannot express ("due in the next hour", "due before this invoice date") from
the same index.

⚠️ **Ask for the soonest deadline with `order by due_at limit 1`, never
`min(due_at)`.** The aggregate-to-index-scan rewrite is refused across a join,
before indexes are considered, so `min()` plans a hash join over two
sequential scans. Measured on a 50 000-instance probe: 6 buffers against 733.
`Engine::next_due_in` carries the same finding for the scheduler's own query,
and `Engine::NEXT_DEADLINE_SQL` is the shape written out.

## `rbpmn_v_subscription`

The question arrives in one shape: someone quotes a business identifier — an
order number, a ticket reference — and asks what is waiting on it. Nobody asks
by instance id; if that were known the answer would already be at hand.

| column | |
|---|---|
| `instance_id`, `subscription_no` | identity |
| `definition_key`, `definition_version`, `element_id` | where in which model |
| `message_name` | the message it is armed for |
| **`correlation_key`** | **the business identifier it is waiting on** |
| `instance_status` | whether `correlate` is answering for it |
| `created_at` | when it was armed |

**The delivery rule, because the view cannot be read correctly without it.**
`correlate` matches on (`message_name`, `correlation_key`) among **active
instances only**, and then: exactly one match delivers; none is
`NoSubscription` (404); two or more is `AmbiguousCorrelation` (409), refused
rather than delivered to an arbitrary one.

An incident-frozen instance keeps its subscriptions, and they neither answer
for a key nor block delivery to a live instance sharing it. That is what
`instance_status` is for — one column between "nothing is waiting" and "the
thing waiting is frozen", which are opposite answers to give someone.

There is no `deliverable` boolean, for the reason `rbpmn_v_timer` has no
`overdue`: it would be `instance_status = 'active'` and nothing else.

The one fact that is *not* derivable from a single row is ambiguity — seeing
it needs an aggregate over the table, which would stop this being an inlinable
projection. So it is a query, and it is the one to run after a 409
(`Engine::AMBIGUOUS_CORRELATIONS_SQL`):

```sql
select message_name, correlation_key, count(*), array_agg(instance_id)
  from rbpmn_v_subscription
 where instance_status = 'active'
 group by 1, 2 having count(*) > 1
```

⚠️ **Search by `correlation_key` and the engine's own correlate index will
not serve you well** — `rbpmn_subscription_correlate` is `(message_name,
correlation_key)`, so a key-only predicate has no leading equality to seek on.
Skip scan gives it one — on PostgreSQL 18 and up — by seeking once per
distinct message name, so the cost grows with your model portfolio; below 18
there is no index path for that predicate at all. Migration 0017 adds
`rbpmn_subscription_by_key` for exactly this. Measured on 60 000
subscriptions: 24 buffers at 4 distinct message names, 394 at 400, against 3
through the explicit index either way.

## SQL, or a typed call?

Use **SQL against the views** whenever the answer involves your own data:
joining your rows, filtering by your tenancy, grouping by your dimensions,
ordering by your rules. That is a join, and it is why the surface is SQL.

Use a **typed call** when you want ids or counts and no join:

- `Engine::queue_depths(definition_keys)` — the dashboard query, busiest
  first. The key set is an argument bound into the statement, and there is
  deliberately **no limit at all**, so nothing is truncated before your filter
  can compose with it. An empty slice matches no keys and returns no rows —
  plain SQL set semantics.
- `Engine::find_by_shared_index(field, value, limit)` — index-backed by
  construction; it refuses outright rather than sequential-scanning when no
  shared index for the field exists.

Timers and subscriptions get no typed call at all; what they get instead is
their query written out — `Engine::NEXT_DEADLINE_SQL`,
`Engine::WAITING_ON_KEY_SQL`, `Engine::AMBIGUOUS_CORRELATIONS_SQL` — because
these are queries with no rule in them that an application wants joined to its
own row anyway.

**The caution that decides between them:** `find_by_shared_index` applies its
limit *in the database, before you see anything*. An application that then
filters the result — by tenant, by permission, by anything — is filtering a
page that was already truncated, and can silently miss rows it was entitled
to. A call that bounds before you can filter is the wrong tool for a filtered
result set; express the filter in SQL against the view, where your predicate
and the bound compose in the right order. `queue_depths` takes no limit for
exactly this reason.

Conditions inside the XML are pure FEEL (a strict subset), so they carry no
rbpmn-specific syntax either. Null follows FEEL exactly: `x = null` is the
"is it set?" test, while `x = 1` **and** `x != 1` are both false when `x` is
missing — a type mismatch is null in either direction, so an unset variable
never satisfies a negative condition. `just feel-parity` differentials the
whole subset against dsntk (DMN-TCK-verified) to keep that honest.

