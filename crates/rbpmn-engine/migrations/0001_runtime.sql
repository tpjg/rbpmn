-- Single shared schema (design brief: schema-per-definition was considered
-- and rejected; partial indexes deliver per-definition isolation later).
-- Every relation is rbpmn_-prefixed: the engine shares its schema with the
-- embedding application's business tables (same-transaction stepping), so
-- generic names like "instance" are not ours to claim.
-- Timer and subscription tables arrive with phase 3 in their own migration.

create table rbpmn_definition (
    id            uuid primary key default gen_random_uuid(),
    key           text not null,
    version       int  not null,
    content_hash  text not null,
    bpmn_xml      text not null,
    bindings      jsonb not null,
    deployed_at   timestamptz not null default now(),
    unique (key, version)
);

create table rbpmn_instance (
    id              uuid primary key default gen_random_uuid(),
    definition_id   uuid not null references rbpmn_definition (id),
    definition_key  text not null,
    business_key    text,
    status          text not null check (status in ('active', 'completed', 'terminated', 'failed')),
    variables       jsonb not null,
    next_token      bigint not null default 0,
    next_work_item  bigint not null default 0,
    created_at      timestamptz not null default now(),
    completed_at    timestamptz
);
create index rbpmn_instance_by_key on rbpmn_instance (definition_key, status);

-- Reserved for embedded subprocesses (v2): the schema is stable from day one
-- even though phase-1/2 instances only ever have the implicit root scope.
create table rbpmn_scope (
    id               uuid primary key default gen_random_uuid(),
    instance_id      uuid not null references rbpmn_instance (id) on delete cascade,
    parent_scope_id  uuid references rbpmn_scope (id),
    element_id       text
);

-- The runtime truth: one row per parked token. Tokens reference element ids,
-- not positional indexes (instance migration depends on this).
create table rbpmn_token (
    instance_id   uuid not null references rbpmn_instance (id) on delete cascade,
    token_no      bigint not null,
    element_id    text not null,
    wait_kind     text not null check (wait_kind in ('join', 'work_item')),
    arrived_via   text,
    work_item_no  bigint,
    primary key (instance_id, token_no)
);

create table rbpmn_work_item (
    id              uuid primary key default gen_random_uuid(),
    instance_id     uuid not null references rbpmn_instance (id) on delete cascade,
    item_no         bigint not null,
    definition_id   uuid not null,
    definition_key  text not null,
    token_no        bigint not null,
    kind            text not null check (kind in ('service', 'user')),
    topic           text not null,
    element_id      text not null,
    state           text not null check (state in ('available', 'locked', 'completed', 'cancelled', 'failed')),
    retries         int not null default 3,
    lock_owner      text,
    lock_until      timestamptz,
    created_at      timestamptz not null default now(),
    unique (instance_id, item_no)
);
create index rbpmn_work_item_acquire on rbpmn_work_item (topic) where state = 'available';

-- The append-only history: the only history mechanism there is.
create table rbpmn_event (
    id              bigserial primary key,
    instance_id     uuid not null,
    definition_id   uuid not null,
    definition_key  text not null,
    kind            text not null,
    element_id      text,
    payload         jsonb not null,
    at              timestamptz not null default now()
);
create index rbpmn_event_by_instance on rbpmn_event (instance_id, id);
