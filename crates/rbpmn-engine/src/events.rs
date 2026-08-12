//! The event-stream tailing contract (phase 5).
//!
//! Two guarantees, stated and kept — and one caveat, stated because it is
//! real rather than because it is comfortable:
//!
//! 1. **Per instance, ascending `id` is the semantic order.** An instance's
//!    steps serialize on its row lock, so each step's event rows are
//!    inserted (and their ids allocated) strictly after the previous step
//!    committed. Sort an instance's events by `id` and you have exactly the
//!    order the core emitted them, always.
//! 2. **A [`Engine::read_events`] cursor never misses an event.** Neither
//!    `id` nor `txid` alone is commit order: ids are assigned at insert but
//!    transactions commit out of order, and a transaction's `txid` is
//!    assigned at its *first write* — a business transaction around an
//!    `*_in_tx` call can hold an old txid while inserting late, high-id
//!    events. The stream is therefore ordered by **(txid, id)** and reads
//!    stop at the *safe horizon*: only rows whose `txid` is older than
//!    every transaction still in progress are returned. Every transaction
//!    below the horizon has terminated, so nothing can ever appear behind a
//!    released `(txid, id)` frontier.
//!
//! **Caveat: stream order is not per-instance order.** Those two are
//! different sort keys and cannot be reconciled in one pass — the cursor
//! must lead with `txid` to be safe, while per-instance semantics live in
//! `id`. So when a long-lived caller transaction takes its xid *before*
//! acquiring an instance's row lock, its (semantically later) events carry
//! a lower txid and arrive *earlier* in the stream than events of a step
//! that really happened first. Consumers that care about per-instance
//! ordering must sort each instance's events by `id` rather than trusting
//! arrival order (a projection keyed by instance is the normal shape, and
//! `id` is carried on every record for exactly this). Autocommit callers —
//! everything over HTTP — never trigger it: their xid and their lock are
//! taken in the same statement.
//!
//! **Retention interacts with exactly one of these guarantees, and loudly**
//! (phase 7). Deleted history cannot be read, so guarantee 2 is restated
//! rather than abandoned: retention advances a monotonic **floor** — the
//! highest `(txid, id)` it has ever deleted — and a resume from a cursor
//! *below* the floor fails with [`EngineError::CursorTruncated`] (HTTP 410)
//! instead of silently skipping the gap. Everything deleted is at or below
//! the floor, so a cursor at or above it has provably lost nothing, however
//! scattered the deleted set. A zero cursor still means "from the
//! beginning", which now reads as *from the oldest retained event*: a new
//! consumer has no completeness expectation to violate, and only a **resume**
//! across a truncation must be loud. The floor may leap over events a lagging
//! consumer had in fact already read (retention deletes whole instances, so
//! the gap is scattered rather than a prefix) — conservative in the safe
//! direction, and only reachable by a consumer lagging further behind than
//! the entire retention age.
//!
//! The horizon is **cluster-wide** (transaction ids are global to the
//! PostgreSQL cluster, not per database): one long-running transaction —
//! anyone's, in any database of the cluster — holds it back. The engine's
//! own transactions are short by design ("commit promptly, the instance row
//! is locked"), and the same discipline applies to business transactions
//! sharing `*_in_tx` calls. Delayed means late, never lost or reordered.

use crate::{Engine, EngineError};
use sqlx::Row;
use uuid::Uuid;

/// A position in the event stream. `Default` is the beginning. Advance by
/// taking the last returned record's [`EventRecord::cursor`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventCursor {
    /// The writing transaction's 64-bit id (xid8; never wraps).
    pub txid: i64,
    pub id: i64,
}

/// One event from the append-only stream, exactly as persisted.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventRecord {
    /// Unique per event, and **ascending in semantic order within one
    /// instance** — the key to sort by when reassembling an instance's
    /// history (see the module docs' caveat).
    pub id: i64,
    /// The writing transaction's id — the stream's major sort key.
    pub txid: i64,
    pub instance_id: Uuid,
    pub definition_key: String,
    pub kind: String,
    pub element_id: Option<String>,
    pub payload: serde_json::Value,
    /// RFC 3339 UTC, database time.
    pub at: String,
}

impl EventRecord {
    /// The cursor to resume after this record.
    pub fn cursor(&self) -> EventCursor {
        EventCursor {
            txid: self.txid,
            id: self.id,
        }
    }
}

impl Engine {
    /// Tail the event stream: up to `limit` events past `after`, in
    /// (txid, id) order, stopping at the safe horizon (see the module docs
    /// — no event can ever appear behind a returned batch). Pass the last
    /// record's [`EventRecord::cursor`] as the next `after`; an empty
    /// result means "nothing final yet", not "nothing will come".
    pub async fn read_events(
        &self,
        after: EventCursor,
        limit: u32,
    ) -> Result<Vec<EventRecord>, EngineError> {
        // Validate at the boundary: a negative cursor would reach Postgres
        // as an invalid xid8 (a 500 on 16+, a silently dead cursor before
        // that), and limit 0 would page forever through empty results that
        // the contract tells the caller to read as "nothing final yet".
        if after.txid < 0 || after.id < 0 {
            return Err(EngineError::InvalidVariables(
                "event cursor components must not be negative".to_string(),
            ));
        }
        if limit == 0 {
            return Err(EngineError::InvalidVariables(
                "event page limit must be at least 1".to_string(),
            ));
        }
        // A *resume* below the floor is a truncation and must be loud. A
        // zero cursor is not a resume: it means "from the oldest retained".
        // Checked before the page query rather than folded into it, because
        // truncation matters most exactly when the page comes back empty.
        if after != EventCursor::default() {
            let floor = self.retention_floor().await?;
            if after < floor {
                return Err(EngineError::CursorTruncated {
                    cursor: after,
                    floor,
                });
            }
        }
        let rows = sqlx::query(
            "select id, txid::text::bigint as txid, instance_id, definition_key, \
             kind, element_id, payload, \
             to_char(at at time zone 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') as at \
             from rbpmn_event \
             where (txid, id) > ($1::text::xid8, $2) \
               and txid < pg_snapshot_xmin(pg_current_snapshot()) \
             order by txid, id limit $3",
        )
        .bind(after.txid.to_string())
        .bind(after.id)
        .bind(i64::from(limit.min(1000)))
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| EventRecord {
                id: r.get("id"),
                txid: r.get("txid"),
                instance_id: r.get("instance_id"),
                definition_key: r.get("definition_key"),
                kind: r.get("kind"),
                element_id: r.get("element_id"),
                payload: r.get("payload"),
                at: r.get("at"),
            })
            .collect())
    }
}
