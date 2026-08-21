-- The published read-only surface for message subscriptions:
-- `rbpmn_v_subscription`. The fourth wait state, and the last one.
--
-- The support question arrives in one shape: someone quotes a business
-- identifier — an order number, a ticket reference — and asks what is waiting
-- on it. That is `correlation_key`, so it is the column this view exists to
-- be searched by, and the index below is what makes searching it viable.
--
-- Same contract as the other three: columns may be added, never removed or
-- repurposed; a plain inlinable projection — no WHERE, no LIMIT, no DISTINCT,
-- no ORDER BY, no aggregate, no volatile function, and explicitly NOT
-- `security_barrier`. It joins `rbpmn_instance` because `rbpmn_subscription`
-- carries no definition coordinates of its own, and composes with
-- `rbpmn_v_instance` on `instance_id`.
--
-- ---------------------------------------------------------------- promises
--
-- A row is an **armed** subscription, not a claim on a message. `correlate`
-- resolves without a lock, then locks the instance and re-checks the row
-- under it, so what this view shows is what was armed when it was read.
-- Reading it reserves nothing and delivers nothing.
--
-- The delivery rule, stated because the view cannot be read correctly
-- without it: `correlate` matches on (`message_name`, `correlation_key`)
-- among **active instances only**, and then
--
--   * exactly one match  -> delivered;
--   * none               -> `NoSubscription` (HTTP 404);
--   * two or more        -> `AmbiguousCorrelation` (HTTP 409) — refused
--                           rather than delivered to an arbitrary one.
--
-- An incident-frozen instance keeps its subscription rows (frozen for
-- repair), and they neither answer for a key nor block delivery to a live
-- instance sharing it. That is why `instance_status` is a column: the row a
-- support question finds may be exactly the one `correlate` is ignoring, and
-- one column is the difference between "nothing is waiting" and "the thing
-- waiting is frozen".
--
-- There is deliberately **no `deliverable` boolean**, for the same reason
-- `rbpmn_v_timer` has no `overdue`: it would be `instance_status = 'active'`
-- and nothing else. A boolean earns its place when it encodes a rule a caller
-- would get wrong — `rbpmn_v_work_item.claimable` folds a lease, a backoff
-- and an instance status into one answer — and this is a single comparison on
-- a column already here.
--
-- The one genuinely non-derivable fact, ambiguity, cannot be a column either:
-- seeing it requires an aggregate over the whole table, and an aggregate
-- would stop this being an inlinable projection. It is a query instead, and
-- the one to run after a 409:
--
--     select message_name, correlation_key, count(*), array_agg(instance_id)
--       from rbpmn_v_subscription
--      where instance_status = 'active'
--      group by 1, 2
--     having count(*) > 1;
create view rbpmn_v_subscription as
select
    s.instance_id,
    s.subscription_no,
    i.definition_key,
    i.definition_version,
    s.element_id,
    s.message_name,
    s.correlation_key,
    i.status as instance_status,
    s.created_at
from rbpmn_subscription s
join rbpmn_instance i on i.id = s.instance_id;

comment on view rbpmn_v_subscription is
    'Public read-only projection of armed message subscriptions. Stable API: columns may be added, never removed or repurposed. Plain inlinable view by design. A read model, not a claim: correlate matches (message_name, correlation_key) among active instances only, refuses two matches rather than picking one, and reading this reserves nothing.';

-- Searching by the business identifier ALONE is the whole point of the view,
-- and the correlate index cannot serve it: `rbpmn_subscription_correlate` is
-- (message_name, correlation_key), so a predicate on the second column with
-- nothing on the first has no leading equality to seek on.
--
-- Two independent reasons, and it is worth having both written down because
-- either alone invites deleting the index as redundant:
--
--   * Btree skip scan is PostgreSQL 18. Below it there is no index path for a
--     key-only predicate on this index at all — development here runs 18, CI
--     runs 15, and the floor the engine claims is 13, so the version that
--     makes the correlate index look sufficient is the one version this is
--     least entitled to assume.
--   * Even on 18, skip scan makes it usable rather than good: it seeks once
--     per distinct prefix value, so the cost scales with the number of
--     distinct message names — the application's model portfolio, which
--     grows. Measured on 60 000 subscriptions: 24 buffers at 4 message names,
--     394 at 400. The explicit index takes 3 either way, because there is
--     nothing to skip over.
--
-- An index that is fast on the developer's laptop and a sequential scan on the
-- deployment's Postgres is worse than no index, because nothing reports it.
--
-- Plain `create index`, not CONCURRENTLY, for the reason 0008 gives: the
-- migration runner applies every migration inside one transaction. On a large
-- existing deployment, build it CONCURRENTLY by hand first and this statement
-- finds it present.
create index if not exists rbpmn_subscription_by_key
    on rbpmn_subscription (correlation_key);
