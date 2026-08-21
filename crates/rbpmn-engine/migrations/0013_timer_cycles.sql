-- Repeating timers (timeCycle) on non-interrupting boundary events
-- (docs/design/boundary-messages.md, slice 3).
--
-- A cycle is one row at a time: the armed occurrence. When it fires, the
-- step deletes this row and inserts the next one in the same transaction, on
-- the grid of this row's due_at — the first `due_at + k * period` at or after
-- now, with k at least 1. The grid is the *previous due's*, never the time
-- the fire happened to run, so a scheduler that was late does not drift the
-- schedule; and because k is at least 1, an engine that was down does not
-- replay the occurrences it missed as a burst. The arithmetic is in epoch
-- seconds: the
-- period is fixed-length by lint (weeks, days, hours, minutes, seconds — never
-- months or years), and `timestamptz + interval '1 day'` would be a calendar
-- day in the session's time zone, which is not what a fixed-length cycle says
-- across a daylight-saving change.
--
-- `remaining` is the core's fire count (`R3/…` starts at 3; the armed one is
-- included, so 1 is the last), null for an unbounded `R/…` and for every
-- non-cycle row. It is state the core owns and the projection only stores —
-- so the column says what the core may store: positive, and only on a cycle.
-- A zero would be a row armed for an occurrence that can never fire, and a
-- count on a `duration` row would be a count nothing ever decrements; the
-- loader rejects both rather than reading them, and these say so at the one
-- place that cannot be bypassed. (This migration is unreleased — it lands on
-- this branch with the feature — so the checks are edited into it in place
-- rather than added as an 0014 nobody would ever apply separately.)

alter table rbpmn_timer drop constraint rbpmn_timer_due_kind_check;
alter table rbpmn_timer add constraint rbpmn_timer_due_kind_check
    check (due_kind in ('duration', 'date', 'cycle'));

alter table rbpmn_timer add column if not exists remaining int;

alter table rbpmn_timer add constraint rbpmn_timer_remaining_check
    check (remaining is null or remaining > 0);
alter table rbpmn_timer add constraint rbpmn_timer_remaining_kind_check
    check (due_kind = 'cycle' or remaining is null);
