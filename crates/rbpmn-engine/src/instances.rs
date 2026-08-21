//! The published read surface over process instances.
//!
//! Two ways in, for two different needs. Applications that must *join* — a
//! result set of "their tenancy, their ordering, rbpmn's instances" is a SQL
//! join and nothing that returns data instead of SQL does it as well — go
//! through the `rbpmn_v_instance` view (migration 0014), which is public API
//! and deliberately a plain inlinable projection so declared variable indexes
//! still apply beneath it. Applications that just want ids for one identifier
//! call [`Engine::find_by_shared_index`] and write no SQL at all.

use crate::{Engine, EngineError};
use sqlx::Row;
use uuid::Uuid;

/// The published view. Named here so callers can build SQL against the
/// contract rather than hard-coding rbpmn's table names, and so a rename
/// would be a compile error somewhere rather than a runtime surprise.
pub const INSTANCE_VIEW: &str = "rbpmn_v_instance";

/// One instance carrying a looked-up value.
///
/// Deliberately thin: identity, which definition it belongs to, and the
/// business key it was started with. Anything else — the variable document,
/// status, timestamps — is a join away through [`INSTANCE_VIEW`], and putting
/// it here would make every lookup pay for a payload most callers discard.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceMatch {
    pub id: Uuid,
    pub definition_key: String,
    pub business_key: Option<String>,
}

/// The largest page [`Engine::find_by_shared_index`] will return. A lookup by
/// business identifier that matches thousands of instances is a different
/// query than this one, and should be written as SQL against the view.
pub const MAX_FIND_LIMIT: u32 = 1000;

impl Engine {
    /// Resolve a business identifier to the instances carrying it, across
    /// every definition — the lookup [`Engine::declare_shared_index`] exists
    /// for, for callers who would rather not write SQL.
    ///
    /// Index-backed **by construction**: the emitted predicate is exactly the
    /// shared index's expression, and the call refuses outright when no
    /// shared index for `field` exists rather than quietly sequential-scanning
    /// every instance in the system. Declare it (`Bindings::shared_index`, or
    /// `declare_shared_index`) and call again.
    ///
    /// Ordered oldest-first (`created_at`, tie-broken by `id`) so a bounded
    /// result is deterministic, and *not* filtered by status — the whole point
    /// is to find whichever instance carries the value, including one that has
    /// already completed.
    ///
    /// **Not a search primitive.** `limit` is applied by the database before
    /// the caller sees anything, so an application that then filters the
    /// result — by tenant, by permission, by anything — is filtering a page
    /// that was already truncated, and can silently miss rows it was entitled
    /// to. Applications doing that must express their filter *in* the query,
    /// against [`INSTANCE_VIEW`], where their predicate and the limit compose
    /// in the right order.
    pub async fn find_by_shared_index(
        &self,
        field: &str,
        value: &str,
        limit: u32,
    ) -> Result<Vec<InstanceMatch>, EngineError> {
        crate::tasks::validate_field(field)?;
        crate::runtime::reject_nul_text(value, "lookup value")?;
        if limit == 0 || limit > MAX_FIND_LIMIT {
            return Err(EngineError::InvalidVariables(format!(
                "limit must be between 1 and {MAX_FIND_LIMIT}, got {limit}"
            )));
        }
        let index = crate::tasks::shared_index_name(field);
        let ready: Option<bool> = sqlx::query_scalar(
            "select i.indisvalid from pg_class c \
             join pg_index i on i.indexrelid = c.oid where c.relname = $1",
        )
        .bind(&index)
        .fetch_optional(self.pool())
        .await?;
        if ready != Some(true) {
            return Err(EngineError::UndeclaredSharedIndex {
                field: field.to_string(),
                index,
            });
        }
        // The field is a literal for the same reason the filter compiler
        // embeds one: the planner needs it to match the index expression.
        // `validate_field` above is what makes that safe.
        let rows = sqlx::query(&format!(
            "select id, definition_key, business_key from {INSTANCE_VIEW} \
             where variables->>'{field}' = $1 \
             order by created_at, id limit $2"
        ))
        .bind(value)
        .bind(i64::from(limit))
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| InstanceMatch {
                id: row.get("id"),
                definition_key: row.get("definition_key"),
                business_key: row.get("business_key"),
            })
            .collect())
    }
}
