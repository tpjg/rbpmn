-- Repeating timers (timeCycle) on non-interrupting boundary events
-- (docs/design/boundary-messages.md, slice 3).
--
-- A cycle is one row at a time: the armed occurrence. When it fires, the
-- step deletes this row and inserts the next one in the same transaction,
-- with due_at = this row's due_at + the period — stepped from the *previous
-- due*, never from the time the fire happened to run, so a scheduler that was
-- late does not drift the schedule. The arithmetic is in epoch seconds: the
-- period is fixed-length by lint (weeks, days, hours, minutes, seconds — never
-- months or years), and `timestamptz + interval '1 day'` would be a calendar
-- day in the session's time zone, which is not what a fixed-length cycle says
-- across a daylight-saving change.
--
-- `remaining` is the core's fire count (`R3/…` starts at 3; the armed one is
-- included, so 1 is the last), null for an unbounded `R/…` and for every
-- non-cycle row. It is state the core owns and the projection only stores.

alter table rbpmn_timer drop constraint rbpmn_timer_due_kind_check;
alter table rbpmn_timer add constraint rbpmn_timer_due_kind_check
    check (due_kind in ('duration', 'date', 'cycle'));

alter table rbpmn_timer add column if not exists remaining int;
