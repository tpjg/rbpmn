-- When the instance froze (docs/design/incident-scope.md, D8). Its clock stops
-- while it is frozen: the repair that lands re-arms its timers by the outage
-- measured from here. Every freeze sets it, a repair that freezes the instance
-- again included, and the step that leaves the freeze clears it.
alter table rbpmn_instance add column frozen_at timestamptz;

-- An instance frozen before this migration froze when its incident was
-- raised, and the event says when.
update rbpmn_instance i set frozen_at = (
    select max(e.at) from rbpmn_event e
    where e.instance_id = i.id and e.kind = 'incident-raised')
where i.status = 'failed';
