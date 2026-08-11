-- Phase 5: the event-stream tailing contract. `id` (bigserial) is assigned
-- at insert time but transactions commit out of order, so `where id > $last`
-- alone can silently skip events whose lower ids commit late. The writing
-- transaction's 64-bit id (xid8 — never wraps) makes an exact safe horizon
-- possible: rows with txid older than every in-progress transaction are
-- final — nothing can ever appear behind them.

alter table rbpmn_event add column txid xid8 not null default pg_current_xact_id();

-- The stream is ordered and cursored by (txid, id): txid order alone is not
-- commit order, but the "txid < oldest in-progress" horizon guarantees no
-- row can ever appear behind a released (txid, id) frontier. Per-instance
-- semantic order is `id` alone (steps serialize on the instance row lock,
-- so their inserts do too) — a different sort key from the stream's, see
-- the caveat in events.rs.
create index rbpmn_event_stream on rbpmn_event (txid, id);

-- `at` is public API on the event stream, so it must be statement time, not
-- transaction-start time: an *_in_tx caller that begins at 10:00:00 and
-- steps at 10:00:45 would otherwise stamp its events 45 seconds early, out
-- of line with both real time and its own (txid, id) position.
alter table rbpmn_event alter column at set default clock_timestamp();
