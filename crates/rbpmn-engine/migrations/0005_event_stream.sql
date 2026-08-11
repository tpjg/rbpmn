-- Phase 5: the event-stream tailing contract. `id` (bigserial) is assigned
-- at insert time but transactions commit out of order, so `where id > $last`
-- alone can silently skip events whose lower ids commit late. The writing
-- transaction's 64-bit id (xid8 — never wraps) makes an exact safe horizon
-- possible: rows with txid older than every in-progress transaction are
-- final — nothing can ever appear behind them.

alter table rbpmn_event add column txid xid8 not null default pg_current_xact_id();

-- The stream is ordered and cursored by (txid, id): txid order alone is not
-- commit order, but the "txid < oldest in-progress" horizon guarantees no
-- row can ever appear behind a released (txid, id) frontier — and within an
-- instance (txid, id) order equals semantic order, because steps serialize
-- on the instance row lock and xids are allocated monotonically.
create index rbpmn_event_stream on rbpmn_event (txid, id);
