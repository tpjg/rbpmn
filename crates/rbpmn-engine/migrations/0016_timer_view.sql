-- The published read-only surface for timers: `rbpmn_v_timer`. The third wait
-- state, and the last one without a contract.
--
-- Instances got a view because an application's result set is a join it has to
-- write itself. Work items got one because a queue depth was reachable only
-- one queue at a time. Timers are asked a different question, just as often:
-- **when does this next happen?** A deadline an application shows a user — a
-- renewal date, a payment reminder, an escalation that has not fired yet —
-- lived only in `rbpmn_timer`, so answering it meant reading an undocumented
-- table, which is what publishing the other two views was meant to end. The
-- same surface answers two operational questions: what is armed at all for an
-- instance (support), and how much is past due (health).
--
-- Same contract as the other two: columns may be added, never removed or
-- repurposed, and it stays a **plain inlinable projection** — no WHERE, no
-- LIMIT, no DISTINCT, no ORDER BY, no aggregate, no volatile function, and
-- explicitly NOT `security_barrier`, so an outside predicate still reaches
-- `rbpmn_timer_due` and the primary key underneath. It joins
-- `rbpmn_instance` because `rbpmn_timer` carries no definition coordinates of
-- its own, and composes with `rbpmn_v_instance` on `instance_id`.
--
-- ---------------------------------------------------------------- promises
--
-- WHAT A ROW IS: an **armed** timer. Not a promise about when it fires.
--
--   * A `due_at` in the past means "due and not yet fired", NOT "late". The
--     scheduler runs on its own cadence and fires at most one timer per pass,
--     so a small backlog is normal operation, not an incident.
--   * Whether the scheduler will take this one next depends on two things
--     beyond `due_at`: the instance must be active (exposed here as
--     `instance_status`, so the health query can separate "scheduler behind"
--     from "instance frozen" without a second join), and the instance must
--     not be in a node's transient deferral set — which is in-process,
--     per-node and invisible to any view. So this surface can say what is
--     armed and due; it cannot say what fires next, and does not pretend to.
--   * For a **cycle**, the row is the NEXT occurrence and never the series.
--     A cycle is one row at a time: firing deletes this row and inserts the
--     next in the same transaction (see 0013). So an application showing a
--     user a date is never choosing between rows — there is exactly one per
--     armed cycle — and `remaining` is how many fires are left including this
--     one (null for an unbounded `R/…`, and null on every non-cycle row).
--
-- ------------------------------------------------------------ two columns
--
-- `due_spec` is the load-bearing one and the reason this is a view rather
-- than "just query due_at". It is the literal the arm resolved from — an
-- ISO-8601 duration or date, or a FEEL qualified name naming one in the
-- variable document — and for a cycle it carries the period too, inside the
-- repetition (`R3/PT1H`). An operator asking "why is it due THEN" needs the
-- source of the instant, not the instant.
--
-- There is deliberately **no `overdue` boolean**. It would be legal — `now()`
-- is STABLE, which is what makes the work-item view's `claimable` legal — but
-- it would encode no rule. `claimable` earns its place because a caller
-- re-deriving it gets lapsed leases or frozen instances wrong; `overdue`
-- is `due_at < now()` and nothing else. A caller should compare `due_at`
-- directly, which also gets them the range queries a boolean cannot express
-- ("due in the next hour", "due before this invoice date") from the same
-- index. Adding it would be a second thing to keep consistent with the first
-- for no rule anyone could get wrong.
--
-- --------------------------------------------------------------- indexing
--
-- No new index. Both questions this view exists for are already served by the
-- scheduler's, and this says which so the next reader does not re-derive it:
--
--   * "next due for this instance" -> `rbpmn_timer_pkey (instance_id,
--     timer_no)`; the leading column is the equality, and the handful of rows
--     an instance can have are sorted after.
--   * "everything overdue right now" -> `rbpmn_timer_due (due_at)`.
--
-- ONE TRAP, MEASURED, and it survives the view: ask for the soonest deadline
-- with `min(due_at)` and Postgres plans a hash join over two sequential
-- scans. `min()` over a join cannot become an index scan — the optimization
-- is refused before indexes are considered — which is the same finding
-- `Engine::next_due_in` records for the scheduler's own query. Write
-- `order by due_at limit 1` instead. Probe, 50 000 instances / 20 000 timers:
-- 6 buffers against 733.
create view rbpmn_v_timer as
select
    t.instance_id,
    t.timer_no,
    i.definition_key,
    i.definition_version,
    t.element_id,
    t.due_kind,
    t.due_spec,
    t.due_at,
    t.remaining,
    i.status as instance_status,
    t.created_at
from rbpmn_timer t
join rbpmn_instance i on i.id = t.instance_id;

comment on view rbpmn_v_timer is
    'Public read-only projection of armed timers. Stable API: columns may be added, never removed or repurposed. Plain inlinable view by design. A read model, not a schedule: a row is what is armed, a due_at in the past means due-and-not-yet-fired, and for a cycle the row is the next occurrence rather than the series. Use order by due_at limit 1, never min(due_at).';
