//! The event-stream tailing contract (phase 5).
//!
//! Two guarantees, stated and kept:
//!
//! 1. **Per instance, `id` order is the semantic order.** Every event of an
//!    instance is written under its instance row lock, so within one
//!    instance, ascending `id` equals commit order equals the order the pure
//!    core emitted.
//! 2. **A [`Engine::read_events`] cursor never misses an event.** `id` is
//!    assigned at insert but transactions commit out of order — a naive
//!    `where id > $last` skips events whose lower ids commit late. Reads
//!    therefore stop at the *safe horizon*: only rows whose writing
//!    transaction (`txid`, a 64-bit xid8) is older than every transaction
//!    still in progress are returned. Behind that horizon nothing can ever
//!    appear, so `last returned id` is always a safe cursor.
//!
//! The horizon is **cluster-wide** (transaction ids are global to the
//! PostgreSQL cluster, not per database): one long-running transaction —
//! anyone's, in any database of the cluster — holds it back. The engine's
//! own transactions are short by design ("commit promptly, the instance row
//! is locked"), and the same discipline applies to business transactions
//! sharing `*_in_tx` calls.

use crate::{Engine, EngineError};
use sqlx::Row;
use uuid::Uuid;

/// One event from the append-only stream, exactly as persisted.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventRecord {
    /// The tailing cursor: strictly increasing per read, totally ordered
    /// per instance.
    pub id: i64,
    pub instance_id: Uuid,
    pub definition_key: String,
    pub kind: String,
    pub element_id: Option<String>,
    pub payload: serde_json::Value,
    /// RFC 3339 UTC, database time.
    pub at: String,
}

impl Engine {
    /// Tail the event stream: up to `limit` events with `id > after`, in id
    /// order, stopping at the safe horizon (see the module docs — the
    /// returned batch can never have an event appear behind it later). Pass
    /// the last returned `id` as the next `after`; an empty result means
    /// "nothing final yet", not "nothing will come".
    pub async fn read_events(
        &self,
        after: i64,
        limit: u32,
    ) -> Result<Vec<EventRecord>, EngineError> {
        let rows = sqlx::query(
            "select id, instance_id, definition_key, kind, element_id, payload, \
             to_char(at at time zone 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"') as at \
             from rbpmn_event \
             where id > $1 and txid < pg_snapshot_xmin(pg_current_snapshot()) \
             order by id limit $2",
        )
        .bind(after)
        .bind(i64::from(limit.min(1000)))
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| EventRecord {
                id: r.get("id"),
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
