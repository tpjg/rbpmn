-- Retry pacing, failure diagnostics, a claim-path index, and persisted
-- environment topic declarations: the deploys a declaration unblocks persist,
-- so the declaration must persist too — a restart or a replica resumes the
-- same environment (code/config still contributes; the union wins).

alter table rbpmn_work_item add column retry_at timestamptz;
alter table rbpmn_work_item add column failures int not null default 0;
alter table rbpmn_work_item add column last_failure text;

create index rbpmn_work_item_claim on rbpmn_work_item (topic, created_at)
    where kind = 'service' and state in ('available', 'locked');

create table rbpmn_environment_topic (
    name         text primary key,
    declared_at  timestamptz not null default now()
);
