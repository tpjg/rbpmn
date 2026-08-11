-- Phase 3: timers and messages. A sleeping timer is a passive row — never a
-- per-timer in-process wait; `due_at` is computed from **database time** at
-- arm time (node clocks never decide anything). A subscription row is an
-- open message wait, addressed by (message_name, correlation_key).

create table rbpmn_timer (
    instance_id  uuid not null references rbpmn_instance (id) on delete cascade,
    timer_no     bigint not null,
    token_no     bigint not null,
    element_id   text not null,
    due_kind     text not null check (due_kind in ('duration', 'date')),
    due_spec     text not null,
    due_at       timestamptz not null,
    created_at   timestamptz not null default now(),
    primary key (instance_id, timer_no)
);
create index rbpmn_timer_due on rbpmn_timer (due_at);

create table rbpmn_subscription (
    instance_id      uuid not null references rbpmn_instance (id) on delete cascade,
    subscription_no  bigint not null,
    token_no         bigint not null,
    element_id       text not null,
    message_name     text not null,
    correlation_key  text not null,
    created_at       timestamptz not null default now(),
    primary key (instance_id, subscription_no)
);
create index rbpmn_subscription_correlate
    on rbpmn_subscription (message_name, correlation_key);

alter table rbpmn_instance add column next_timer bigint not null default 0;
alter table rbpmn_instance add column next_subscription bigint not null default 0;

alter table rbpmn_token drop constraint rbpmn_token_wait_kind_check;
alter table rbpmn_token add constraint rbpmn_token_wait_kind_check
    check (wait_kind in ('join', 'work_item', 'timer', 'message', 'event_gateway', 'incident'));
