//! The event-stream tailing contract (phase 5).
//!
//! Two guarantees, stated and kept:
//!
//! 1. **Per instance, stream order is the semantic order.** Every event of
//!    an instance is written under its instance row lock, so its steps'
//!    transactions serialize — and because xids are allocated monotonically,
//!    a later step always carries a higher `txid`. Within one transaction,
//!    ascending `id` is the emission order.
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
    /// Unique per event; ascending per instance within one transaction.
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
