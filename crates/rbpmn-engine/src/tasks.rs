//! The pull-mode task API: external workers and user-task frontends claim
//! work by topic (`FOR UPDATE SKIP LOCKED`), hold it under a short renewable
//! lease, and complete/fail it owner-checked. Pull and push mode share
//! `rbpmn_work_item` — same rows, same lease model, same completion path.
//!
//! Ordering is FIFO by default (`created_at` ascending, tie-broken by item
//! number — a fair queue; `created_at` is database time, so it is consistent
//! across nodes) or LIFO opt-in (freshest-first triage). Honesty note from
//! the design brief: under concurrent consumers FIFO is fair-but-not-strict
//! — `SKIP LOCKED` skips rows a peer is claiming; strict global FIFO would
//! serialize all consumers, the wrong trade for a work queue.
//!
//! Filters compare fields of the owning instance's **live** variables — the
//! single variable document, never a snapshot that could silently diverge.
//! The filter compiler emits exactly the expression shape that
//! [`Engine::declare_index`] indexes (`variables->>'field'` with a literal
//! `definition_key` predicate), so declared indexes actually serve the
//! query; undeclared fields stay correct via sequential scan, just slower.

use crate::{Completion, Engine, EngineError, FailOptions, FailOutcome};
use sqlx::Row;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskOrder {
    /// Oldest first (`created_at` ascending, tie-broken by item number).
    #[default]
    Fifo,
    /// Freshest first — triage mode.
    Lifo,
}

/// Equality filters over the owning instance's live variables, ANDed.
/// Definition-scoped on purpose: fields mean something per definition, and
/// the literal `definition_key` is what lets the planner use the partial
/// indexes `declare_index` creates.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskFilter {
    definition_key: String,
    fields: BTreeMap<String, String>,
}

impl TaskFilter {
    pub fn new(definition_key: impl Into<String>) -> Self {
        TaskFilter {
            definition_key: definition_key.into(),
            fields: BTreeMap::new(),
        }
    }

    /// Require `variables->>'field' = value` (text comparison of the JSON
    /// text form, exactly what the declared index indexes).
    pub fn field(mut self, field: impl Into<String>, value: impl Into<String>) -> Self {
        self.fields.insert(field.into(), value.into());
        self
    }
}

#[derive(Debug, Clone)]
pub struct GetTaskOptions {
    /// Lease holder identity, recorded in `lock_owner`; completion and
    /// failure of a live-leased task require it.
    pub owner: String,
    /// Lease TTL; renew via [`Engine::extend_lock`] while working.
    pub ttl: Duration,
    pub order: TaskOrder,
    pub filter: Option<TaskFilter>,
}

impl GetTaskOptions {
    pub fn new(owner: impl Into<String>) -> Self {
        GetTaskOptions {
            owner: owner.into(),
            ttl: Duration::from_secs(600),
            order: TaskOrder::Fifo,
            filter: None,
        }
    }
}

/// A claimed task: everything a worker or task UI needs to act on it.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LockedTask {
    pub id: Uuid,
    pub instance_id: Uuid,
    pub definition_key: String,
    pub element_id: String,
    pub topic: String,
    pub kind: String,
    /// The instance's live variables as of the claim.
    pub variables: serde_json::Value,
    /// Lease deadline (RFC 3339 UTC, database time). Renew before it.
    pub lock_until: String,
}

/// The typed heartbeat result: a client whose lease was lost must be able to
/// tell its user "this task was reassigned" — never fail silently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockExtension {
    Extended {
        until: String,
    },
    /// Owner mismatch, expired lease, or the task is already closed.
    Lost,
}

/// A lease must be plausible: zero would mint a lock expired at birth (two
/// owners on one task moments later), and an absurd TTL turns into a
/// Postgres interval error surfaced as a 500. Reject both at the boundary.
fn validate_ttl(ttl: Duration) -> Result<(), EngineError> {
    const MIN_TTL: Duration = Duration::from_millis(10);
    const MAX_TTL: Duration = Duration::from_secs(30 * 24 * 3600);
    if ttl < MIN_TTL || ttl > MAX_TTL {
        return Err(EngineError::InvalidVariables(format!(
            "lease ttl must be between 10ms and 30 days, got {ttl:?}"
        )));
    }
    Ok(())
}

/// Field names embed in SQL as literals (the planner needs the literal to
/// match the index expression), so they are validated hard — same segment
/// grammar as FEEL qualified names.
fn validate_field(field: &str) -> Result<(), EngineError> {
    let ok = !field.is_empty()
        && field
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && field.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !ok {
        return Err(EngineError::InvalidVariables(format!(
            "'{field}' is not a valid filter/index field name \
             ([A-Za-z_][A-Za-z0-9_]*)"
        )));
    }
    Ok(())
}

/// Up-front validation for a manifest index entry (deploy calls this before
/// anything persists).
pub(crate) fn validate_index_declaration(key: &str, field: &str) -> Result<(), EngineError> {
    validate_definition_key(key)?;
    validate_field(field)
}

/// Definition keys embed in SQL as literals too (partial-index predicate).
/// BPMN ids are XML NCNames; we accept the safe subset and reject the rest.
pub(crate) fn validate_definition_key(key: &str) -> Result<(), EngineError> {
    let ok = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.');
    if !ok {
        return Err(EngineError::InvalidVariables(format!(
            "'{key}' is not a safe definition key for filtering/indexing \
             ([A-Za-z0-9_.-]+)"
        )));
    }
    Ok(())
}

/// Compiles a filter to SQL predicates over the instance alias `i`. Emits
/// EXACTLY the declared-index expression shape: literal definition_key,
/// literal field name, parameterized value.
fn compile_filter(
    filter: &TaskFilter,
    args: &mut Vec<String>,
    first_param: usize,
) -> Result<String, EngineError> {
    validate_definition_key(&filter.definition_key)?;
    let mut sql = format!(" and i.definition_key = '{}'", filter.definition_key);
    for (field, value) in &filter.fields {
        validate_field(field)?;
        crate::runtime::reject_nul_text(value, "filter value")?;
        write!(
            sql,
            " and i.variables->>'{field}' = ${}",
            first_param + args.len()
        )
        .expect("write to string");
        args.push(value.clone());
    }
    Ok(sql)
}

impl Engine {
    /// Claim the next task on `topic` (any kind — pull-mode service workers
    /// and user-task frontends share this API). `None` when nothing is
    /// claimable right now. Single-statement claim: atomic, `SKIP LOCKED`
    /// arbitrates competing consumers, expired leases count as available.
    pub async fn get_task(
        &self,
        topic: &str,
        options: &GetTaskOptions,
    ) -> Result<Option<LockedTask>, EngineError> {
        crate::runtime::reject_nul_text(topic, "topic")?;
        crate::runtime::reject_nul_text(&options.owner, "owner")?;
        validate_ttl(options.ttl)?;
        let direction = match options.order {
            TaskOrder::Fifo => "asc",
            TaskOrder::Lifo => "desc",
        };
        let mut args: Vec<String> = Vec::new();
        let filter_sql = match &options.filter {
            Some(filter) => compile_filter(filter, &mut args, 4)?,
            None => String::new(),
        };
        let sql = format!(
            "update rbpmn_work_item set state = 'locked', lock_owner = $2, \
             lock_until = now() + make_interval(secs => $3) \
             where id = (select w.id from rbpmn_work_item w \
                join rbpmn_instance i on i.id = w.instance_id \
                where w.topic = $1 and {claimable}{filter_sql} \
                order by w.created_at {direction}, w.item_no {direction} \
                limit 1 for update of w skip locked) \
             returning id, instance_id, definition_key, element_id, topic, kind, \
               to_char(lock_until at time zone 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') \
                 as lock_until, \
               (select variables from rbpmn_instance i2 \
                 where i2.id = rbpmn_work_item.instance_id) as variables",
            claimable = crate::CLAIMABLE,
        );
        let mut query = sqlx::query(&sql)
            .bind(topic)
            .bind(&options.owner)
            .bind(options.ttl.as_secs_f64());
        for value in &args {
            query = query.bind(value);
        }
        let Some(row) = query.fetch_optional(self.pool()).await? else {
            return Ok(None);
        };
        // Claim and variables read are ONE statement (the subquery in the
        // RETURNING list): a step committing between two statements can no
        // longer hand back post-claim variables — one claim, one snapshot.
        let instance_id: Uuid = row.get("instance_id");
        let variables: serde_json::Value = row.get("variables");
        Ok(Some(LockedTask {
            id: row.get("id"),
            instance_id,
            definition_key: row.get("definition_key"),
            element_id: row.get("element_id"),
            topic: row.get("topic"),
            kind: row.get("kind"),
            variables,
            lock_until: row.get("lock_until"),
        }))
    }

    /// How many tasks on `topic` are claimable right now (dashboard
    /// indications). Same predicates and filter shape as [`Engine::get_task`].
    pub async fn count_tasks(
        &self,
        topic: &str,
        filter: Option<&TaskFilter>,
    ) -> Result<u64, EngineError> {
        crate::runtime::reject_nul_text(topic, "topic")?;
        let mut args: Vec<String> = Vec::new();
        let filter_sql = match filter {
            Some(filter) => compile_filter(filter, &mut args, 2)?,
            None => String::new(),
        };
        let sql = format!(
            "select count(*) from rbpmn_work_item w \
             join rbpmn_instance i on i.id = w.instance_id \
             where w.topic = $1 and {claimable}{filter_sql}",
            claimable = crate::CLAIMABLE,
        );
        let mut query = sqlx::query(&sql).bind(topic);
        for value in &args {
            query = query.bind(value);
        }
        let count: i64 = query.fetch_one(self.pool()).await?.get(0);
        Ok(count as u64)
    }

    /// Heartbeat: extend the lease while demonstrably still working. A lost
    /// lease (owner mismatch, expiry, task closed) is a typed result, not an
    /// error — the client tells its user "this task was reassigned".
    pub async fn extend_lock(
        &self,
        task: Uuid,
        owner: &str,
        ttl: Duration,
    ) -> Result<LockExtension, EngineError> {
        crate::runtime::reject_nul_text(owner, "owner")?;
        validate_ttl(ttl)?;
        let row = sqlx::query(
            "update rbpmn_work_item set lock_until = now() + make_interval(secs => $3) \
             where id = $1 and lock_owner = $2 and state = 'locked' and lock_until > now() \
             returning to_char(lock_until at time zone 'UTC', \
               'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') as lock_until",
        )
        .bind(task)
        .bind(owner)
        .bind(ttl.as_secs_f64())
        .fetch_optional(self.pool())
        .await?;
        Ok(match row {
            Some(row) => LockExtension::Extended {
                until: row.get("lock_until"),
            },
            None => LockExtension::Lost,
        })
    }

    /// Owner-checked completion: refuses while another holder's lease is
    /// live (`ItemLeased`), otherwise identical to
    /// [`Engine::complete_work_item`] — including the idempotent
    /// `AlreadyClosed` no-op, so a retried completion converges. The lease
    /// guard runs *under the instance lock* inside `complete_work_item_in_tx`
    /// — the one lock order engine-wide (instance row, then item row).
    pub async fn complete_task(
        &self,
        task: Uuid,
        owner: &str,
        patch: serde_json::Value,
    ) -> Result<Completion, EngineError> {
        let mut tx = self.pool().begin().await?;
        let completion = self
            .complete_work_item_in_tx(&mut tx, task, Some(owner), patch)
            .await?;
        tx.commit().await?;
        Ok(completion)
    }

    /// Owner-checked failure — [`Engine::fail_work_item`] with the caller's
    /// lease identity filled in.
    pub async fn fail_task(
        &self,
        task: Uuid,
        owner: &str,
        error_code: Option<String>,
        detail: Option<String>,
    ) -> Result<FailOutcome, EngineError> {
        self.fail_work_item(
            task,
            &FailOptions {
                error_code,
                detail,
                owner: Some(owner.to_string()),
            },
        )
        .await
    }
}

/// Deterministic index name from (definition key, field) — the same call is
/// idempotent and re-runnable at startup. Hash-based when the readable form
/// would overflow Postgres's 63-byte identifier limit.
fn index_name(definition_key: &str, field: &str) -> String {
    let clean: String = definition_key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let name = format!("rbpmn_vix_{clean}_{field}");
    if name.len() <= 63 {
        return name;
    }
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(definition_key.as_bytes());
    hasher.update([0]);
    hasher.update(field.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    let mut name = format!("rbpmn_vix_{}_{field}", &digest[..16]);
    name.truncate(63);
    name
}

impl Engine {
    /// Declare that the application filters/counts tasks of `definition_key`
    /// by this variables field: creates the partial expression index the
    /// filter compiler's shape matches. **Entirely optional performance
    /// API** — filtering works without it (sequential scan is correct, just
    /// slower). Idempotent; safe to re-run at startup next to handler
    /// registration. Predicated on `definition_key`, which is stable across
    /// definition versions.
    ///
    /// Built `CONCURRENTLY` (no lock on running work), which cannot run
    /// inside a transaction and, on failure, leaves an *invalid* index
    /// behind that `IF NOT EXISTS` would silently accept forever — so
    /// validity is verified after the build and an invalid leftover is
    /// dropped and reported loudly instead.
    pub async fn declare_index(
        &self,
        definition_key: &str,
        field: &str,
    ) -> Result<(), EngineError> {
        validate_definition_key(definition_key)?;
        validate_field(field)?;
        let name = index_name(definition_key, field);
        sqlx::query(&format!(
            "create index concurrently if not exists {name} on rbpmn_instance \
             ((variables->>'{field}')) where definition_key = '{definition_key}'"
        ))
        .execute(self.pool())
        .await?;
        let valid: Option<bool> = sqlx::query_scalar(
            "select i.indisvalid from pg_class c \
             join pg_index i on i.indexrelid = c.oid where c.relname = $1",
        )
        .bind(&name)
        .fetch_optional(self.pool())
        .await?;
        match valid {
            Some(true) => Ok(()),
            Some(false) => {
                // A previously interrupted concurrent build: drop the
                // corpse so the next call can rebuild, and say so.
                sqlx::query(&format!("drop index concurrently if exists {name}"))
                    .execute(self.pool())
                    .await?;
                Err(EngineError::InvalidVariables(format!(
                    "index '{name}' was left invalid by an interrupted build; \
                     it has been dropped — call declare_index again"
                )))
            }
            None => Err(EngineError::InvalidVariables(format!(
                "index '{name}' disappeared during creation"
            ))),
        }
    }
}
