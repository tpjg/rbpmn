-- The pinned definition *version*, stored where the instance and its work
-- items already store the pinned definition id and key.
--
-- An instance pins a definition at start and never migrates, so
-- (definition_id, definition_key, definition_version) is immutable for its
-- whole life — and two of those three were already denormalised onto both
-- rbpmn_instance and rbpmn_work_item since 0001. This adds the third to the
-- same two rows, for the same reason: the claim path needs the pair an
-- embedding application resolves version-pinned per-task metadata by, and
-- reaching rbpmn_definition for it on every claim is a lookup for a value
-- that cannot change.
--
-- It replaces a correlated subquery in `get_task`'s RETURNING list. That
-- subquery was correct but paid twice over: once per claim on the hot pull
-- path, and once in review, because `rbpmn_work_item.definition_id` carries
-- no foreign key — nothing at the schema level guaranteed the subquery found
-- a row, so a NULL would have surfaced as a decode panic inside an HTTP
-- handler rather than as an error. A NOT NULL column cannot.
--
-- The backfill is also the assertion. If any row's definition has gone
-- missing, `set not null` fails and the migration aborts, loudly, in the
-- transaction that would otherwise have shipped the assumption. It should
-- not happen: a work item cannot outlive its instance (instance_id cascades)
-- and retention refuses to delete a definition any instance still
-- references. That argument is now checked once rather than trusted forever.
--
-- Cost on an existing deployment: one full-table UPDATE per table, inside
-- the migration transaction, rewriting every row. For a large rbpmn_instance
-- that is not free — it is the price of the column, paid once, and the same
-- trade every ALTER ... SET NOT NULL in these migrations makes.

alter table rbpmn_instance add column if not exists definition_version int;
update rbpmn_instance i set definition_version = d.version
    from rbpmn_definition d
    where d.id = i.definition_id and i.definition_version is null;
alter table rbpmn_instance alter column definition_version set not null;

alter table rbpmn_work_item add column if not exists definition_version int;
update rbpmn_work_item w set definition_version = d.version
    from rbpmn_definition d
    where d.id = w.definition_id and w.definition_version is null;
alter table rbpmn_work_item alter column definition_version set not null;
