//! The published read surface over work items — the queue side of the
//! projection, beside [`crate::instances`]'s instance side.
//!
//! The motivating question is a triage screen's first paint: *for every queue
//! this user can work, how many items are waiting right now?*, busiest first,
//! so someone opening the application knows which backlog to attack. The same
//! number feeds capacity planning, alerting and SLA reporting, so it is asked
//! continuously, by every user, on the first screen they load.
//!
//! [`Engine::count_tasks`] answers it one queue at a time — one topic, one
//! definition-scoped filter — so a dashboard covering T topics across D
//! deployed definitions costs T×D round trips. Through
//! [`WORK_ITEM_VIEW`] it is one statement, and it joins
//! [`crate::instances::INSTANCE_VIEW`] on `instance_id` so an application can
//! group depths by its *own* dimensions in that same statement.

use crate::{Engine, EngineError};
use sqlx::Row;

/// The published work-item view. Named here, beside
/// [`crate::INSTANCE_VIEW`], so callers build SQL against the contract rather
/// than rbpmn's table names — a rename is then a compile error for them
/// instead of a runtime surprise.
pub const WORK_ITEM_VIEW: &str = "rbpmn_v_work_item";

/// How deep one queue is, for one definition.
///
/// `waiting` and `in_progress` are disjoint by construction and do **not**
/// sum to "open items": an item whose instance froze on an incident is
/// neither — not claimable, and nobody is holding it. That gap is the point,
/// not an omission; a queue with 0 waiting and 5 in progress is a different
/// situation from 0 and 0, and both differ again from 0, 0 and a pile of
/// frozen work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueDepth {
    pub definition_key: String,
    pub topic: String,
    /// Items [`Engine::get_task`] would hand out right now.
    pub waiting: u64,
    /// Items held under a lease that has not expired.
    pub in_progress: u64,
}

impl Engine {
    /// Queue depths for the given definitions, busiest first — the triage
    /// dashboard's query, for callers who would rather not write SQL.
    ///
    /// `waiting` is computed with the *same predicate* [`Engine::get_task`]
    /// claims by, so a depth cannot disagree with what the engine will
    /// actually hand out. It is not `state = 'available'`: a lapsed lease
    /// counts, a live one does not, retry backoff that has not come due does
    /// not, closed states never do, and neither does anything belonging to an
    /// instance frozen on an incident.
    ///
    /// **A read model, not a claim.** A depth is true when it was measured
    /// and can be stale by the time it is rendered; the only way to hold an
    /// item is [`Engine::get_task`]. Two dashboards seeing 5 waiting does not
    /// mean ten items exist.
    ///
    /// `definition_keys` is an **argument, bound as a parameter**, so the
    /// caller's filter is part of the query rather than something applied to
    /// a result that was already cut down. There is deliberately no limit
    /// here at all: this is the shape [`Engine::find_by_shared_index`] warns
    /// about, and the way to not fall into that trap is to have no bound for
    /// a caller-side filter to disagree with. An empty slice matches no keys
    /// and returns no rows — plain SQL set semantics, not a special case; to
    /// span every deployed definition, write the statement against
    /// [`WORK_ITEM_VIEW`].
    pub async fn queue_depths(
        &self,
        definition_keys: &[String],
    ) -> Result<Vec<QueueDepth>, EngineError> {
        for key in definition_keys {
            crate::runtime::reject_nul_text(key, "definition key")?;
        }
        // `(claimable or in_progress)` is not a convenience: it is what lets
        // the planner prove `state in ('available','locked')` and reach
        // `rbpmn_work_item_depth`, instead of aggregating over every work
        // item ever created. The `filter` clauses then split that candidate
        // set in one pass.
        let rows = sqlx::query(&format!(
            "select definition_key, topic, \
               count(*) filter (where claimable) as waiting, \
               count(*) filter (where in_progress) as in_progress \
             from {WORK_ITEM_VIEW} \
             where (claimable or in_progress) and definition_key = any($1) \
             group by definition_key, topic \
             order by waiting desc, definition_key, topic"
        ))
        .bind(definition_keys)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| QueueDepth {
                definition_key: row.get("definition_key"),
                topic: row.get("topic"),
                waiting: row.get::<i64, _>("waiting") as u64,
                in_progress: row.get::<i64, _>("in_progress") as u64,
            })
            .collect())
    }
}
