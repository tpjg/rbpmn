-- Phase 7: retention. Two knobs, one floor, and no automatic deletion of
-- anything that does not grow.
--
-- Retirement is two-stage. Runtime retention deletes an instance's
-- *children* (tokens, work items, timers, subscriptions, scopes) — the hot
-- rows carrying the claim/due/correlate indexes — and stamps `pruned_at`.
-- The instance row itself survives as the header of its own history, which
-- is what keeps `inspect_instance` working, keeps business keys queryable,
-- and means an event never exists without its instance. History retention
-- then deletes the row and its events together, as one record.

-- Null until runtime retention retires the children. A pruned instance is
-- terminal, has no children, and cannot be stepped.
alter table rbpmn_instance add column pruned_at timestamptz;

-- The two sweep planners, indexed on purpose (the query the engine runs).
-- Separate partial indexes rather than one shared index: the runtime index
-- *shrinks* as instances are pruned (rows leave its predicate), so the
-- runtime sweep never scans the long tail of already-pruned rows that a
-- combined index would accumulate over a year-long history retention.
-- 'failed' is absent from both by design: an incident is frozen evidence
-- and a repair target, never something a sweep may tidy away.
create index rbpmn_instance_prune_runtime on rbpmn_instance (completed_at)
    where pruned_at is null and status in ('completed', 'terminated');
create index rbpmn_instance_prune_history on rbpmn_instance (completed_at)
    where status in ('completed', 'terminated');

-- `delete_definition` asks "does anything still reference this version?",
-- and so does startup re-validation. A referencing-side index is not
-- implied by the foreign key.
create index rbpmn_instance_by_definition on rbpmn_instance (definition_id);

-- The truncation floor: the highest (txid, id) ever deleted, monotonic and
-- never decreasing. Everything deleted is <= floor, so a cursor >= floor has
-- provably lost nothing and a cursor < floor is told so — loudly — instead
-- of silently skipping deleted history. One row, forever.
create table rbpmn_retention_floor (
    only_row boolean primary key default true check (only_row),
    txid     xid8 not null default '0'::xid8,
    id       bigint not null default 0
);
insert into rbpmn_retention_floor (only_row) values (true);

-- Retention is operational, not semantic, so the policy is keyed by
-- definition **key** and not by version: "orders history: 7 years" is a
-- property of the process, not of v3 of the process. Keying by version
-- would force a redeploy to change an operational knob and would let two
-- live versions of one process disagree. Null means "fall back to the
-- sweeper's default policy"; the definition rows stay immutable.
create table rbpmn_retention_policy (
    definition_key      text primary key,
    retain_runtime_secs bigint,
    retain_history_secs bigint,
    updated_at          timestamptz not null default now()
);

-- The sweep's cross-node claim. A *lease* (a row value), never an open
-- transaction and never a session advisory lock: a sweep pass spans three
-- transactions with an archive upload in the gap, and both of those
-- alternatives would either pin `xmin` across a network round-trip (which
-- stalls every event-stream reader in the cluster) or leak the lock forever
-- if the pass is cancelled mid-flight. An expiring lease is self-healing.
-- Same idiom as a work item's lock_owner/lock_until, for the same reason.
create table rbpmn_retention_lease (
    only_row boolean primary key default true check (only_row),
    owner    text,
    until    timestamptz
);
insert into rbpmn_retention_lease (only_row) values (true);
