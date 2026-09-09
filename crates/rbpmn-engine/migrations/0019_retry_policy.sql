-- Per-element retry policy: the curve a failed service item retries on, as
-- deployment data rather than one number per Engine.
-- (`docs/design/retry-policy.md`.)
--
-- Additive and nullable, and NULL is load-bearing: it means *ask the engine*,
-- not "3 and 5 seconds, frozen at insert". Existing rows keep behaving
-- exactly as they do, and `retry_backoff` stays a runtime setting — an
-- operator who lowers the base during an incident window sees it apply to
-- work already queued, which is the whole reason it is one.
--
-- `retries` itself is untouched: same column, same default, same meaning it
-- has always had (the *total* handler calls an item gets, because the fail
-- path decrements and then tests). What changes is that the insert now binds
-- it from the manifest instead of letting the default decide.
--
-- Why these two are columns at all, when `config` deliberately is not
-- (task-config D6): what consumes them. Config is unbounded application JSON
-- read by a claimant, in Rust, after the claim — a column would be a second
-- copy of an arbitrarily large fact, and the definition cache serves it for
-- free. These two are scalars consumed by the `UPDATE` that applies them,
-- beside clock_timestamp() and power(); resolving them in Rust would mean one
-- statement taking its inputs from two places.
alter table rbpmn_work_item add column backoff_base double precision;
alter table rbpmn_work_item add column backoff_multiplier double precision;

comment on column rbpmn_work_item.backoff_base is
    'Seconds before the first retry, from the deployment manifest. NULL means the engine''s configured base, read at fail time.';
comment on column rbpmn_work_item.backoff_multiplier is
    'How fast the gaps widen. NULL means 3, the curve rbpmn has always had.';

-- The published view gains both, **appended after created_at** rather than
-- placed beside `retries`. Two reasons, and the first one is not a
-- preference: `create or replace view` only permits additions at the end. It
-- is also the honest contract — a view's column *order* is part of what
-- `select *` returns, so inserting in the middle would move every column
-- after it for readers who never asked for a change.
--
-- Everything else about 0015 stands and is repeated verbatim below: still a
-- plain inlinable projection, still no WHERE and no aggregate, still not
-- security_barrier, and `claimable` is still the same text as `CLAIMABLE` in
-- src/lib.rs, differentialled by
-- `the_view_and_the_claim_predicate_cannot_drift`.
--
-- They are published rather than left to a join for the reason the views
-- exist at all: the alternative is every application joining
-- rbpmn_v_definition.bindings and re-implementing element-then-topic-then-
-- engine resolution in SQL, and a dashboard whose answer to "why has this not
-- retried yet" disagrees with the engine is worse than no dashboard. NULL
-- reads as "the engine's setting" here exactly as it does on the table.
create or replace view rbpmn_v_work_item as
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
    w.created_at,
    w.backoff_base,
    w.backoff_multiplier
from rbpmn_work_item w
join rbpmn_instance i on i.id = w.instance_id;

comment on view rbpmn_v_work_item is
    'Public read-only projection of work items, with claimability computed by the engine using the same predicate get_task uses. Stable API: columns may be added, never removed or repurposed. Plain inlinable view by design. A read model, not a claim: a depth is true when it was measured, and the only way to hold an item is get_task.';
