-- The published read-only surface: `rbpmn_v_instance`.
--
-- Applications legitimately need to join their own rows against instances —
-- a result set that is "our tenancy, our ordering, their instances" is a SQL
-- join, and no API that returns *data* instead of SQL can do it as well.
--
-- CRITICAL, and the reason for every "not" below: this must stay a **plain,
-- inlinable projection**. One table, no WHERE, no LIMIT, no DISTINCT, no
-- ORDER BY, no volatile function, no aggregate, and explicitly **not**
-- `security_barrier`. A barrier view refuses to push an outside predicate
-- below itself unless the operators involved are leakproof, and `jsonb ->>`
-- is not one — the declared variable indexes would then sit unused beneath a
-- full scan of every instance in the system. `tests/engine.rs` asserts the
-- plan through this view, not just its shape, precisely so a well-meant
-- `security_barrier` cannot land silently.
--
-- Deliberately not exposed: `definition_id` (an internal surrogate; the
-- stable coordinates are key + version) and the `next_*` allocators, which
-- are bookkeeping for the step function and mean nothing outside it.
create view rbpmn_v_instance as
select
    id,
    definition_key,
    definition_version,
    business_key,
    status,
    variables,
    created_at,
    completed_at
from rbpmn_instance;

comment on view rbpmn_v_instance is
    'Public read-only projection of process instances. Stable API: columns may be added, never removed or repurposed. Plain inlinable view by design — declared variable indexes must remain usable through it.';
