-- Phase 7: retention. One age per definition, one floor, and no automatic
-- deletion of anything that does not grow.
--
-- A record retires whole: the instance row, its children and its events go
-- together, in one transaction, after the archive sink (if any) has been
-- handed a complete copy. A two-stage variant was built first — retire the
-- children early, keep the record as its own history header — and then
-- collapsed: a completed instance emits tens of events against a handful of
-- work items, and its tokens, timers, subscriptions and scopes are already
-- gone (completion removes them), so the early stage reclaimed roughly a
-- tenth of the footprint in exchange for a column, an index, a guard on the
-- hot step path, and a second planner. Events dominate; the archive is where
-- long histories belong.

-- `rbpmn_event.instance_id` has been an unenforced reference since 0001,
-- deliberately, so that history could outlive its instance. One-stage
-- retention makes that impossible by construction, so it becomes a real
-- foreign key: the cascade replaces an explicit delete, and "an event never
-- outlives its instance" stops being an invariant this codebase asserts and
-- becomes one the database will not let it break. That is also what keeps
-- `delete_definition`'s "is anything still referencing this?" an indexed
-- lookup on rbpmn_instance instead of a scan of the largest table here.
--
-- The referential-integrity check lands on the highest-volume insert path in
-- the system, which is affordable for a specific reason: a step already
-- holds its instance row FOR UPDATE, so the check's KEY SHARE lock is
-- uncontended — it costs an index probe per event row, nothing more.
alter table rbpmn_event
    add constraint rbpmn_event_instance_fk
    foreign key (instance_id) references rbpmn_instance (id) on delete cascade;

-- The sweep planner, indexed on purpose (the query the engine runs).
-- 'failed' is absent by design: an incident is frozen evidence and a repair
-- target, never something a sweep may tidy away.
create index rbpmn_instance_retire on rbpmn_instance (completed_at)
    where status in ('completed', 'terminated');

-- `delete_definition` asks "does anything still reference this version?",
-- and so does startup re-validation. A referencing-side index is not
-- implied by the foreign key.
create index rbpmn_instance_by_definition on rbpmn_instance (definition_id);

-- How many records of this definition retention has retired. Without it,
-- `delete_definition`'s guard is accidentally void exactly when it matters:
-- the guard counts live instance rows, retention exists to remove them, and
-- an archived record carries element ids but no BPMN — so the definition
-- that explains them becomes deletable the moment the history is exported.
-- Definitions grow with deployments, not throughput (a handful of versions
-- per process, a few KB each), so the answer is not to copy the XML into
-- every archived record: it is to let the guard say *why* it refuses.
alter table rbpmn_definition add column retired_instances bigint not null default 0;

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
-- live versions of one process disagree. Null means *forever* — and the
-- row's presence, not its nullness, is what decides whether the sweeper's
-- default applies. The definition rows stay immutable.
create table rbpmn_retention_policy (
    definition_key text primary key,
    retain_secs    bigint check (retain_secs is null or retain_secs >= 0),
    updated_at     timestamptz not null default now()
);

-- The sweep's cross-node claim. A *lease* (a row value), never an open
-- transaction and never a session advisory lock: a sweep pass spans two
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
