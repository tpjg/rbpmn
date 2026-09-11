-- Repair (docs/design/incident-scope.md, D7 and D9).
--
-- An incident freezes exactly one token as its cause, parked at 'incident'.
-- Everything else the freeze stops is collateral: 'halted', a move that had
-- not entered its node, with the flow it was on in arrived_via because a
-- parallel join counts arrivals by incoming flow; or 'halted_decision', a
-- pending decision that resumes by asking again. Rows written before this
-- read as 'incident', the safe reading: nothing resumes a token whose shape
-- it cannot tell.
alter table rbpmn_token drop constraint rbpmn_token_wait_kind_check;
alter table rbpmn_token add constraint rbpmn_token_wait_kind_check
    check (wait_kind in ('join', 'work_item', 'timer', 'message',
                         'event_gateway', 'incident', 'scope',
                         'halted', 'halted_decision'));

-- Each freeze mints the next incident number, and a repair names the one it
-- repairs (D9). An instance frozen before this migration holds the only
-- incident it ever raised, so that one is number 0 and the counter stands
-- at 1.
alter table rbpmn_instance add column next_incident bigint not null default 0;
update rbpmn_instance set next_incident = 1 where status = 'failed';
