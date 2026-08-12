-- Phase 6: embedded subprocesses. The scope tree goes live.
--
-- `rbpmn_scope` was reserved in 0001 with a uuid key, before the runtime
-- model existed; it has never held a row (nothing referenced it). It is
-- replaced here with the shape the core actually produces: per-instance
-- numeric ids, exactly like tokens, work items, timers and subscriptions,
-- so rehydration is uniform across every kind of runtime state.

drop table rbpmn_scope;

create table rbpmn_scope (
    instance_id      uuid not null references rbpmn_instance (id) on delete cascade,
    scope_no         bigint not null,
    parent_scope_no  bigint not null,
    element_id       text not null,
    -- The parked token that resumes when this scope completes.
    token_no         bigint not null,
    primary key (instance_id, scope_no)
);

-- Tokens live in a scope; 0 is the instance root, which has no row.
alter table rbpmn_token add column scope_no bigint not null default 0;

alter table rbpmn_instance add column next_scope bigint not null default 1;

alter table rbpmn_token drop constraint rbpmn_token_wait_kind_check;
alter table rbpmn_token add constraint rbpmn_token_wait_kind_check
    check (wait_kind in ('join', 'work_item', 'timer', 'message',
                         'event_gateway', 'incident', 'scope'));
