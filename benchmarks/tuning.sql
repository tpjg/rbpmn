-- Per-table storage parameters for the benchmark database.
--
-- The engine's own migrations set none of these, deliberately: they are
-- operational tuning for a workload the engine cannot see, and a library
-- that silently rewrote a host application's autovacuum policy would be
-- overstepping. The design brief does say to plan them from the start
-- ("job-ish tables are churn-heavy") — so this is where they are planned,
-- measured, and recorded in every result file (`postgres.table_options`).
--
-- Applied by the harness on every run, idempotent, and hashed into the
-- result so a number can never be attributed to settings it did not run
-- under.
--
-- Why these two tables:
--   rbpmn_work_item  — every claim UPDATEs a row (state, lock_owner,
--                      lock_until) and every heartbeat UPDATEs it again.
--                      One instance's five tasks produce far more row
--                      versions than five.
--   rbpmn_token      — a token row is written when it moves and deleted
--                      when it leaves; a completed instance's tokens are
--                      all dead tuples.
-- Both accumulate dead tuples far faster than the 20% default scale factor
-- reacts to, and a bloated claim index is exactly what turns a fast
-- SKIP LOCKED claim into a slow one.

alter table rbpmn_work_item set (
    autovacuum_vacuum_scale_factor = 0.02,
    autovacuum_vacuum_threshold = 200,
    autovacuum_analyze_scale_factor = 0.02,
    autovacuum_vacuum_cost_delay = 0,
    fillfactor = 85
);

alter table rbpmn_token set (
    autovacuum_vacuum_scale_factor = 0.02,
    autovacuum_vacuum_threshold = 200,
    autovacuum_analyze_scale_factor = 0.02,
    autovacuum_vacuum_cost_delay = 0
);

-- Append-only, and the largest table in the schema. Nothing is updated, so
-- the only reason to vacuum is the visibility map and freezing; analyze
-- often enough that the planner knows how big it got.
alter table rbpmn_event set (
    autovacuum_vacuum_scale_factor = 0.1,
    autovacuum_analyze_scale_factor = 0.02,
    fillfactor = 100
);

-- Updated on every step (the variable document) and read by every filtered
-- inbox query. fillfactor leaves room for HOT updates, which keep the
-- declared partial indexes from being rewritten on every patch.
alter table rbpmn_instance set (
    autovacuum_vacuum_scale_factor = 0.05,
    autovacuum_analyze_scale_factor = 0.02,
    fillfactor = 85
);
