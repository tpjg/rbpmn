-- The published read-only surface for work items: `rbpmn_v_work_item`.
--
-- Instances got a view because an application's result set is a join it has
-- to write itself. Work items get one for a sharper reason: the number a
-- triage screen is built on — "for every queue this user can work, how many
-- items are waiting right now?" — was only reachable one queue at a time,
-- because `count_tasks` takes a single topic and a single definition-scoped
-- filter. A dashboard covering T topics across D definitions was T×D round
-- trips. It is one statement now, and it composes with `rbpmn_v_instance` on
-- `instance_id`, so depths can be grouped by an application's own dimensions
-- (a tenant in the definition key, a hoisted variable) in that same
-- statement.
--
-- Same contract as `rbpmn_v_instance`: columns may be added, never removed or
-- repurposed, and it must stay a **plain inlinable projection** — no WHERE,
-- no LIMIT, no DISTINCT, no ORDER BY, no aggregate, no volatile function, and
-- explicitly NOT `security_barrier`. (`now()` below is STABLE, not volatile,
-- which is what makes a time-dependent column legal here at all.) A barrier
-- view would refuse to push an outside predicate below itself, and the
-- grouped depth query would stop using the index underneath. tests/engine.rs
-- asserts the *plan* through this view, not just its shape.
--
-- WHY THE JOIN: claimability is not a property of the work item alone. An
-- instance frozen on an incident keeps its work items exactly where they
-- were, and they must not be handed out — so `i.status = 'active'` is part of
-- the rule, and the view has to reach the instance to tell the truth. A
-- simple join view is still inlined; the plan test proves it.
--
-- WHY THESE TWO COLUMNS ARE COMPUTED HERE: `claimable` is NOT
-- `state = 'available'`. It has to account for a lapsed lease (claimable
-- again), a live lease (not), retry backoff not yet due (not), closed states
-- (never) and a frozen instance (never). If the view exposed only raw
-- columns, every application would re-derive that rule, and a dashboard whose
-- depths disagree with what `get_task` actually hands out is worse than no
-- dashboard. The expression below is the same text as `CLAIMABLE` in
-- src/lib.rs, which the claim path uses; a migration is static SQL and cannot
-- read a Rust const, so `the_view_and_the_claim_predicate_cannot_drift`
-- differentials them row for row instead of trusting the copy.
--
-- Deliberately not exposed: `definition_id` (an internal surrogate — the
-- stable coordinates are key + version), `token_no` (step-function
-- bookkeeping) and `lease_no` (the claim protocol's epoch, meaningful only to
-- a holder mid-claim).
create view rbpmn_v_work_item as
select
    w.id,
    w.instance_id,
    w.item_no,
    w.definition_key,
    w.definition_version,
    w.element_id,
    w.topic,
    w.kind,
    w.state,
    (w.state = 'available'
     or (w.state = 'locked' and w.lock_until is not null and w.lock_until < now()))
    and (w.retry_at is null or w.retry_at <= now())
    and i.status = 'active'                                            as claimable,
    (w.state = 'locked' and w.lock_until is not null and w.lock_until >= now())
                                                                       as in_progress,
    w.lock_owner,
    w.lock_until,
    w.retry_at,
    w.retries,
    w.failures,
    w.last_failure,
    w.created_at
from rbpmn_work_item w
join rbpmn_instance i on i.id = w.instance_id;

comment on view rbpmn_v_work_item is
    'Public read-only projection of work items, with claimability computed by the engine using the same predicate get_task uses. Stable API: columns may be added, never removed or repurposed. Plain inlinable view by design. A read model, not a claim: a depth is true when it was measured, and the only way to hold an item is get_task.';

-- The depth index, and an honest account of what it does. Partial on the open
-- states because closed items dominate the table forever and none of them can
-- be waiting; leading on `definition_key` because that is what a caller
-- filters by — a user works *some* queues, not all of them.
--
-- It is NOT what saves the unfiltered `group by definition_key, topic`: the
-- pre-existing `rbpmn_work_item_fifo` carries the same partial predicate, and
-- with nothing to filter on the planner reasonably picks that instead. Where
-- this index earns its keep is the filtered shape `queue_depths` issues —
-- measured, `definition_key` becomes an index condition rather than a filter,
-- and the instance join collapses from hashing every instance to a nested
-- loop on the primary key. `the_grouped_depth_query_is_index_driven` asserts
-- both halves separately, and by property (no sequential scan, driven by a
-- partial index) for the unfiltered one, because which partial index wins
-- there is the planner's business.
--
-- Either way the planner has to prove `state in ('available','locked')` from
-- the claimable expression, which is the whole reason that expression must
-- not be wrapped in COALESCE — measured: with the wrapper it proves nothing
-- and the query becomes a parallel sequential scan of the entire table.
--
-- Plain `create index`, not CONCURRENTLY, for the reason 0008 gives: the
-- migration runner applies every migration inside one transaction. On a large
-- existing deployment, build it CONCURRENTLY by hand first and this statement
-- finds it present.
create index if not exists rbpmn_work_item_depth on rbpmn_work_item (definition_key, topic)
    where state in ('available', 'locked');
