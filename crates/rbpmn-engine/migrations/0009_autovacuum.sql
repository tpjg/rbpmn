-- Per-table autovacuum settings for the runtime tables.
--
-- The design brief asked for these "from the start" — "job-ish tables
-- (work_item, token) are churn-heavy: plan per-table autovacuum settings
-- from the start" — and until now they existed only in the benchmark
-- harness, which is the wrong side of the line. A setting that only the
-- benchmark applies makes the benchmark measure a system nobody runs.
--
-- The engine sets these on its **own** `rbpmn_`-prefixed tables and touches
-- nothing global, so a host application's tables and the server's own
-- configuration are unaffected. Precedent: `declare_index` already has the
-- engine create indexes on the application's behalf, because the access
-- pattern is the engine's knowledge and not the operator's to reverse
-- engineer.
--
-- These are a floor, not a policy. `ALTER TABLE ... SET (...)` from an
-- operator afterwards wins permanently: migrations run once and are recorded
-- in the ledger, so this never re-asserts itself over a considered override.
--
-- Locking: `ALTER TABLE ... SET (storage parameter)` takes SHARE UPDATE
-- EXCLUSIVE — it does not block reads or writes, only concurrent
-- vacuum/analyze/DDL. Unlike an index build, this is safe to apply to a busy
-- table. Note that `fillfactor` affects **new** pages only; it does not
-- rewrite what is already there.
--
-- Every number below is justified by something measured, not by taste.

-- The claim table. Two separate problems, both measured on a
-- million-instance population:
--
--   * Bloat. Completing a work item removes it from the partial claim
--     indexes and leaves a dead entry until vacuum reclaims it; until then
--     every claim walks past all of them. The same claim measured 25.885 ms
--     with the dead entries present and 0.041 ms straight after a VACUUM.
--     The default 0.2 scale factor waits for a fifth of the table to die
--     first — and closed work items accumulate here until retention removes
--     them, so "a fifth of the table" is a large number that grows.
--
--   * Statistics. The claim path's plan depends on the estimated size of
--     this table and of rbpmn_instance; stale statistics flipped it to a
--     nested loop driven from the instance side, measured at 20 against 175
--     instances/sec.
--
-- cost_delay 0 (unthrottled) is safe specifically here: the *live* set is
-- the open items, which is small and bounded by worker throughput, so a pass
-- finishes quickly however large the table has grown. Raise it if this
-- database shares spindles with something latency-sensitive.
--
-- fillfactor leaves room for HOT updates. It does not help the claim itself
-- — that changes `state`, which is a predicate column of two partial
-- indexes, and Postgres refuses HOT when any indexed column is touched — but
-- lease heartbeats update only `lock_until`, which is indexed nowhere, and
-- those are the high-frequency updates a long-held task generates.
alter table rbpmn_work_item set (
    autovacuum_vacuum_scale_factor = 0.02,
    autovacuum_vacuum_threshold = 200,
    autovacuum_analyze_scale_factor = 0.02,
    autovacuum_vacuum_cost_delay = 0,
    fillfactor = 85
);

-- Tokens are created and deleted as they move and as scopes are torn down;
-- a completed instance's tokens are all dead tuples at once. Same reasoning
-- as work_item minus the fillfactor: there is no update-heavy path here to
-- keep HOT-eligible.
alter table rbpmn_token set (
    autovacuum_vacuum_scale_factor = 0.02,
    autovacuum_vacuum_threshold = 200,
    autovacuum_analyze_scale_factor = 0.02,
    autovacuum_vacuum_cost_delay = 0
);

-- Every step updates the variable document, so this table generates one dead
-- tuple per step even though its row count barely moves.
--
-- The analyze factor is the important one and it is the one with a measured
-- failure behind it: the claim path filters on `i.status = 'active'`, and
-- when statistics were last collected while the system was idle, `status`
-- looks 100% completed, the planner estimates that no instance is active,
-- and it drives the join from this table. That measured 20 instances/sec
-- against 175. The default 0.1 factor makes it *worse* the larger the
-- history grows, because the trigger scales with the table while the active
-- working set does not.
--
-- fillfactor keeps variable-document updates HOT-eligible — except on
-- instances of a definition with a `declare_index` field, where the
-- expression index makes every variables update non-HOT by definition.
alter table rbpmn_instance set (
    autovacuum_vacuum_scale_factor = 0.05,
    autovacuum_analyze_scale_factor = 0.02,
    fillfactor = 85
);

-- Append-only, so nothing here is about dead tuples: `vacuum_scale_factor`
-- stays at its default, and retention's bulk deletes are rare and batched.
-- What this table needs is *statistics*, because `id` and `txid` increase
-- monotonically and the event-stream cursor asks for exactly the range above
-- the last analyzed maximum — the classic case where a stale histogram makes
-- the planner underestimate and pick badly. fillfactor is deliberately not
-- set: 100 is already the default, and asserting a default is noise.
alter table rbpmn_event set (
    autovacuum_analyze_scale_factor = 0.02
);

-- Timers and subscriptions are per-token attachments: inserted when armed,
-- deleted when they fire, are cancelled, or their scope is torn down. Same
-- churn shape as tokens, at lower volume, and the scheduler's candidate
-- query reads rbpmn_timer on every drain pass.
alter table rbpmn_timer set (
    autovacuum_vacuum_scale_factor = 0.05,
    autovacuum_analyze_scale_factor = 0.02
);

alter table rbpmn_subscription set (
    autovacuum_vacuum_scale_factor = 0.05,
    autovacuum_analyze_scale_factor = 0.02
);
