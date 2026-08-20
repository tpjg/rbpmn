-- DMN artifacts travel inside a deployment (docs/dmn.md, D4): an instance
-- pins its process version, so it must pin the decisions that were in force
-- too. They are stored raw and re-parsed, exactly like `bpmn_xml` — the
-- engine keeps one copy of the truth and derives the rest.
--
-- `on delete cascade` because a decision cannot outlive the definition that
-- carries it. That is the same reasoning `rbpmn_event.instance_id` got in
-- phase 7: an invariant the database enforces beats one the code asserts.
create table rbpmn_definition_decision (
    definition_id uuid not null references rbpmn_definition (id) on delete cascade,
    -- Artifacts may import one another, so the order they were deployed in is
    -- part of the deployment, not ours to shuffle.
    ordinal       int  not null,
    dmn_xml       text not null,
    primary key (definition_id, ordinal)
);
