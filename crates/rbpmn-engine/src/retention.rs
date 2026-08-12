//! Retention (phase 7): the bounded deletion of history, and nothing else.
//!
//! **Retirement is two-stage, and only the second stage destroys anything.**
//! Runtime retention deletes an instance's *children* — tokens, work items,
//! timers, subscriptions, scopes — and stamps `pruned_at`. That is what
//! reclaims the hot working set (the tables carrying the claim, due and
//! correlate indexes); the instance row itself survives as the header of its
//! own history. History retention then deletes the row and its events
//! together, as one record, after the archive sink (if any) has been handed
//! a complete copy. So an event never outlives its instance row, which keeps
//! `inspect_instance` working across the whole history window and makes
//! "is this definition still referenced?" a single indexed lookup.
//!
//! **Three phases, and the middle one holds no transaction.** A pass is
//! `plan` → `archive` → `execute`. The archive call reaches an object store
//! over the network, and *any* open transaction holds back
//! `pg_snapshot_xmin` — which is cluster-wide, and which is exactly what the
//! event stream's safe horizon is built on. A sink call inside the deletion
//! transaction would stall every tailing consumer in the cluster for the
//! duration of an S3 upload: the feature that archives history would freeze
//! the stream that reads it. Hence the split, and hence the cross-node claim
//! is a *lease row* rather than a session advisory lock (which would leak
//! forever if a pass were cancelled mid-flight) — the same "a lease is a row
//! value, never an open transaction" rule the task API already follows.
//!
//! What makes a three-phase pass safe across the gap is that **retention
//! only ever selects immutable data**: a terminal instance cannot become
//! non-terminal, and its events are append-only and closed. The planned set
//! therefore cannot change under the archive call, so `execute` needs no
//! reconciliation — only a re-check under the row lock, which it does anyway.
//!
//! Failure modes, stated rather than discovered:
//!
//! - **The sink fails → nothing is deleted.** No archive, no deletion, ever.
//!   Storage grows until the sink is fixed; that is the correct bias.
//! - **The sink succeeds, then the process dies → the batch is archived
//!   again next pass.** Export is **at-least-once**; every record carries
//!   its instance id, so an object-per-instance layout makes the retry an
//!   idempotent overwrite. Two sweepers whose leases overlap have the same
//!   effect.
//!
//! **What retention will not do.** It never touches an active instance
//! (rows or events). It never touches a `failed` one at any age — an
//! incident is frozen evidence and a repair target, and a sweep that tidied
//! it away would destroy both. It never deletes a definition, because an
//! automatic sweep is justified by unbounded growth and by nothing else:
//! definitions grow with deployments, not throughput, and a deleted
//! definition turns an archive into a pile of element ids. Definitions go
//! only through [`Engine::delete_definition`], named one at a time, guarded.
//! And retention writes nothing to the event stream — it would grow what it
//! prunes and inject rows for instances that no longer exist.
//!
//! **One contract does narrow, and it is worth knowing.** Completing or
//! failing an already-closed work item is normally an idempotent no-op
//! (`Completion::AlreadyClosed`) rather than an error, because handlers are
//! at-least-once and a late retry must not look like a fault. Once runtime
//! retention has retired that instance's children the row is gone, so the
//! same late retry gets `UnknownWorkItem` instead. `retain_runtime` is
//! therefore also the window in which a straggling worker can still
//! recognise its own completed work — set it comfortably longer than any
//! handler's retry horizon.
//!
//! Because eligibility is evaluated **per instance**, a wedged instance
//! never blocks its neighbours' retirement. The tempting alternative — a
//! global watermark at "the oldest event of any non-terminal instance" — is
//! one number, trivially safe, and permanently jammed by a single stuck
//! instance until the disk fills.

use crate::events::{EventCursor, EventRecord};
use crate::{Engine, EngineError};
use sqlx::{PgConnection, Row};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use uuid::Uuid;

/// How long each stage of a record is kept, measured from `completed_at`.
/// `None` means forever — the default for everything, because deleting data
/// is opted into, never inherited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// How long an instance keeps its runtime children (tokens, work items,
    /// timers, subscriptions, scopes). Reclaims query latency.
    pub retain_runtime: Option<Duration>,
    /// How long the record itself — instance header, variables, events —
    /// is kept. Reclaims storage, and is the only stage that destroys the
    /// sole copy of anything, so it is the one the archive hook covers.
    pub retain_history: Option<Duration>,
}

impl RetentionPolicy {
    /// Keep everything, forever. An explicit and entirely valid choice.
    pub fn forever() -> Self {
        RetentionPolicy {
            retain_runtime: None,
            retain_history: None,
        }
    }

    /// Rejects `retain_runtime > retain_history`: pruning children *after*
    /// the record they belong to has been deleted is not a policy, it is a
    /// typo. `retain_runtime: None` with a finite history is allowed and
    /// coherent — "never prune early, drop the whole record at the end" —
    /// because history retention deletes children too.
    pub fn new(
        retain_runtime: Option<Duration>,
        retain_history: Option<Duration>,
    ) -> Result<Self, EngineError> {
        if let (Some(runtime), Some(history)) = (retain_runtime, retain_history)
            && runtime > history
        {
            return Err(EngineError::InvalidRetentionPolicy(format!(
                "retain_runtime ({runtime:?}) is longer than retain_history ({history:?}) — \
                 the children would outlive the record they belong to"
            )));
        }
        Ok(RetentionPolicy {
            retain_runtime,
            retain_history,
        })
    }

    fn runtime_secs(&self) -> Option<i64> {
        self.retain_runtime.map(|d| d.as_secs() as i64)
    }

    fn history_secs(&self) -> Option<i64> {
        self.retain_history.map(|d| d.as_secs() as i64)
    }
}

/// Sweeper configuration. Deliberately has no `Default`: starting a sweeper
/// means naming the policy it applies, and `RetentionPolicy::forever()` is
/// the way to say "nothing, for now".
#[derive(Clone)]
pub struct RetentionOptions {
    /// Applied to every definition key with no row in
    /// `rbpmn_retention_policy`.
    pub default_policy: RetentionPolicy,
    /// How long to wait after a pass that found nothing.
    pub sweep_interval: Duration,
    /// Instances considered per pass, per stage.
    pub max_instances: u32,
    /// Soft ceiling on events archived+deleted per pass. Never splits an
    /// instance: a record is archived whole or not at all.
    pub max_events: u32,
    /// How long a pass's claim on the sweep is held. A pass that overruns
    /// it may be joined by another node — which costs a duplicate archive
    /// upload, never a lost or double deletion.
    pub lease_ttl: Duration,
}

impl RetentionOptions {
    pub fn new(default_policy: RetentionPolicy) -> Self {
        RetentionOptions {
            default_policy,
            sweep_interval: Duration::from_secs(300),
            max_instances: 200,
            max_events: 20_000,
            lease_ttl: Duration::from_secs(300),
        }
    }
}

/// Why an archive attempt failed. Any failure stops the deletion.
#[derive(Debug, Clone)]
pub struct ArchiveError {
    pub message: String,
}

impl ArchiveError {
    pub fn new(message: impl Into<String>) -> Self {
        ArchiveError {
            message: message.into(),
        }
    }
}

/// A sink for records about to be deleted — object storage, a data
/// warehouse, a compliance log. Called with **no transaction open**, so it
/// may take as long as it needs; nothing is deleted unless it returns `Ok`.
/// Delivery is at-least-once, keyed by [`InstanceRecord::id`].
pub trait RetentionArchive: Send + Sync {
    fn archive<'a>(
        &'a self,
        batch: &'a ArchiveBatch,
    ) -> Pin<Box<dyn Future<Output = Result<(), ArchiveError>> + Send + 'a>>;
}

/// One complete, self-contained instance record: everything that is about to
/// stop existing. Materialised before deletion — a sink must never need the
/// database to interpret what it was handed.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceRecord {
    pub id: Uuid,
    pub definition_key: String,
    pub definition_version: i32,
    pub business_key: Option<String>,
    /// `completed` or `terminated` from a sweep; a sweep never archives a
    /// `failed` instance, but [`Engine::delete_instance`] can.
    pub status: String,
    /// The final variable document.
    pub variables: serde_json::Value,
    pub created_at: String,
    pub completed_at: Option<String>,
    pub pruned_at: Option<String>,
    /// The instance's full history, ascending by `id` — which is its
    /// semantic order (see the [`crate::events`] contract).
    pub events: Vec<EventRecord>,
}

/// The records one pass is about to delete.
#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveBatch {
    pub instances: Vec<InstanceRecord>,
}

impl ArchiveBatch {
    pub fn is_empty(&self) -> bool {
        self.instances.is_empty()
    }

    pub fn event_count(&self) -> usize {
        self.instances.iter().map(|i| i.events.len()).sum()
    }
}

/// What one pass proposes to do. Produced by [`Engine::plan_retention`],
/// consumed by [`Engine::execute_retention`], with the archive in between.
#[derive(Debug, Clone, Default)]
pub struct RetentionBatch {
    runtime_prune: Vec<Uuid>,
    history: ArchiveBatch,
}

impl RetentionBatch {
    pub fn is_empty(&self) -> bool {
        self.runtime_prune.is_empty() && self.history.is_empty()
    }

    /// Instances whose children this pass would retire (the record stays).
    pub fn runtime_prune(&self) -> &[Uuid] {
        &self.runtime_prune
    }

    /// The records this pass would delete outright — what to archive.
    pub fn history(&self) -> &ArchiveBatch {
        &self.history
    }
}

/// What a pass actually did (after row locks and re-checks).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetentionReport {
    /// Instances whose children were retired; the record survives.
    pub instances_pruned: u64,
    /// Records deleted outright.
    pub instances_deleted: u64,
    pub events_deleted: u64,
    /// The truncation floor after the pass.
    pub floor: EventCursor,
}

impl RetentionReport {
    /// Did this pass move anything? Drives the sweeper's sleep, exactly as
    /// `Drain` drives the scheduler's.
    pub fn moved(&self) -> bool {
        self.instances_pruned > 0 || self.instances_deleted > 0
    }
}

/// Whether a definition version can be deleted, and if not, what holds it.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrunableDefinition {
    pub definition_id: Uuid,
    pub key: String,
    pub version: i32,
    /// Instance rows still referencing it — live, terminal or pruned. Each
    /// one is either runtime state or a history record, and both need the
    /// definition to be intelligible.
    pub instances: i64,
    /// `None` when the version is safe to delete.
    pub blocked_by: Option<String>,
}

/// Timestamp rendering shared with the event stream: RFC 3339, UTC,
/// microsecond precision, database time.
const TS: &str = "'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'";

fn ts(column: &str, alias: &str) -> String {
    format!("to_char({column} at time zone 'UTC', {TS}) as {alias}")
}

impl Engine {
    /// The current truncation floor: everything at or below it may have been
    /// deleted. A consumer that has been away resumes here after a
    /// [`EngineError::CursorTruncated`].
    pub async fn retention_floor(&self) -> Result<EventCursor, EngineError> {
        let row = sqlx::query("select txid::text::bigint as txid, id from rbpmn_retention_floor")
            .fetch_one(self.pool())
            .await?;
        Ok(EventCursor {
            txid: row.get("txid"),
            id: row.get("id"),
        })
    }

    /// Set (or clear) the policy for one definition key. Keyed by key, not
    /// version — retention is operational, not semantic. Idempotent.
    pub async fn set_retention_policy(
        &self,
        definition_key: &str,
        policy: RetentionPolicy,
    ) -> Result<(), EngineError> {
        crate::runtime::reject_nul_text(definition_key, "definition key")?;
        sqlx::query(
            "insert into rbpmn_retention_policy \
                 (definition_key, retain_runtime_secs, retain_history_secs) \
             values ($1, $2, $3) \
             on conflict (definition_key) do update \
                 set retain_runtime_secs = excluded.retain_runtime_secs, \
                     retain_history_secs = excluded.retain_history_secs, \
                     updated_at = now()",
        )
        .bind(definition_key)
        .bind(policy.runtime_secs())
        .bind(policy.history_secs())
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// The stored override for a key, if any.
    pub async fn retention_policy(
        &self,
        definition_key: &str,
    ) -> Result<Option<RetentionPolicy>, EngineError> {
        let row = sqlx::query(
            "select retain_runtime_secs, retain_history_secs from rbpmn_retention_policy \
             where definition_key = $1",
        )
        .bind(definition_key)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(|r| RetentionPolicy {
            retain_runtime: r
                .get::<Option<i64>, _>("retain_runtime_secs")
                .map(|s| Duration::from_secs(s.max(0) as u64)),
            retain_history: r
                .get::<Option<i64>, _>("retain_history_secs")
                .map(|s| Duration::from_secs(s.max(0) as u64)),
        }))
    }

    /// Phase one of a pass: read-only, bounded, and — because it only ever
    /// selects immutable data — stable until executed.
    ///
    /// Records are materialised whole, event bodies included, whether or not
    /// a sink is registered: a plan that reported different content depending
    /// on hidden engine state would be a poor thing to hand an operator
    /// driving `plan`/`execute` from their own job runner.
    /// [`RetentionOptions::max_events`] is what keeps that affordable.
    pub async fn plan_retention(
        &self,
        options: &RetentionOptions,
    ) -> Result<RetentionBatch, EngineError> {
        let mut conn = self.pool().acquire().await?;
        let history = plan_history(&mut conn, options).await?;
        let doomed: std::collections::HashSet<Uuid> =
            history.instances.iter().map(|i| i.id).collect();
        // An instance can be due for both stages at once (equal ages, or a
        // first sweep after a long silence). Retiring the children of a
        // record this same pass is about to delete outright is pure work,
        // and it would double-count it in the report.
        let runtime_prune: Vec<Uuid> = plan_runtime(&mut conn, options)
            .await?
            .into_iter()
            .filter(|id| !doomed.contains(id))
            .collect();
        Ok(RetentionBatch {
            runtime_prune,
            history,
        })
    }

    /// Phase three: the short transaction. Re-checks every instance under
    /// its row lock (`skip locked` — an instance being stepped right now is
    /// left for the next pass, never an error), deletes, and advances the
    /// floor to the highest `(txid, id)` it actually removed.
    pub async fn execute_retention(
        &self,
        batch: &RetentionBatch,
    ) -> Result<RetentionReport, EngineError> {
        let mut tx = self.pool().begin().await?;
        let mut report = RetentionReport::default();

        if !batch.runtime_prune.is_empty() {
            let ids: Vec<Uuid> = sqlx::query_scalar(
                "select id from rbpmn_instance \
                 where id = any($1) and pruned_at is null \
                   and status in ('completed', 'terminated') \
                 for update skip locked",
            )
            .bind(&batch.runtime_prune)
            .fetch_all(&mut *tx)
            .await?;
            if !ids.is_empty() {
                report.instances_pruned = prune_children(&mut tx, &ids).await?;
            }
        }

        let history_ids: Vec<Uuid> = batch.history.instances.iter().map(|i| i.id).collect();
        if !history_ids.is_empty() {
            let ids: Vec<Uuid> = sqlx::query_scalar(
                "select id from rbpmn_instance \
                 where id = any($1) and status in ('completed', 'terminated') \
                 for update skip locked",
            )
            .bind(&history_ids)
            .fetch_all(&mut *tx)
            .await?;
            if !ids.is_empty() {
                let (events, instances) = delete_records(&mut tx, &ids).await?;
                report.events_deleted = events;
                report.instances_deleted = instances;
            }
        }

        report.floor = read_floor(&mut tx).await?;
        tx.commit().await?;
        Ok(report)
    }

    /// One complete pass: claim the lease, plan, archive, execute. Returns
    /// an empty report when another node holds the lease or nothing is due.
    /// Deterministic and side-effect-complete — the unit tests drive this,
    /// and so can a cron job that would rather not run a daemon.
    pub async fn sweep_retention_once(
        &self,
        options: &RetentionOptions,
    ) -> Result<RetentionReport, EngineError> {
        let owner = Uuid::new_v4().to_string();
        if !self
            .claim_retention_lease(&owner, options.lease_ttl)
            .await?
        {
            return self.idle_report().await;
        }
        let result = self.sweep_leased(options).await;
        // Best effort: an unreleased lease expires on its own, which is the
        // whole reason this is a lease and not a lock.
        if let Err(e) = self.release_retention_lease(&owner).await {
            tracing::warn!(error = %e, "releasing the retention lease failed; it will expire");
        }
        result
    }

    async fn sweep_leased(
        &self,
        options: &RetentionOptions,
    ) -> Result<RetentionReport, EngineError> {
        let batch = self.plan_retention(options).await?;
        if batch.is_empty() {
            return self.idle_report().await;
        }
        self.run_archive(batch.history()).await?;
        let report = self.execute_retention(&batch).await?;
        tracing::info!(
            pruned = report.instances_pruned,
            deleted = report.instances_deleted,
            events = report.events_deleted,
            floor_txid = report.floor.txid,
            floor_id = report.floor.id,
            "retention pass"
        );
        Ok(report)
    }

    /// A pass that moved nothing still reports the *real* floor. Reporting a
    /// zero floor here would be a lie a consumer could act on ("history
    /// starts at the beginning"), and it would make the report's floor
    /// non-monotonic across passes for no reason.
    async fn idle_report(&self) -> Result<RetentionReport, EngineError> {
        Ok(RetentionReport {
            floor: self.retention_floor().await?,
            ..RetentionReport::default()
        })
    }

    /// Hand a batch to the archive sink, if one is registered. Any failure
    /// is fatal to the pass: no archive, no deletion.
    async fn run_archive(&self, batch: &ArchiveBatch) -> Result<(), EngineError> {
        if batch.is_empty() {
            return Ok(());
        }
        let Some(sink) = self.inner_archive() else {
            return Ok(());
        };
        sink.archive(batch)
            .await
            .map_err(|e| EngineError::ArchiveFailed(e.message))
    }

    /// Runs forever (spawn it; abort to stop). A pass that moved something
    /// loops immediately; anything else sleeps. Transient errors back off —
    /// the loop must survive database restarts, and a failing archive sink
    /// must not turn into a hot retry loop.
    pub async fn run_retention(&self, options: RetentionOptions) {
        loop {
            let wait = match self.sweep_retention_once(&options).await {
                Ok(report) if report.moved() => continue,
                Ok(_) => options.sweep_interval,
                Err(e) => {
                    tracing::warn!(error = %e, "retention pass failed; backing off");
                    options.sweep_interval.min(Duration::from_secs(60))
                }
            };
            tokio::time::sleep(wait.max(Duration::from_secs(1))).await;
        }
    }

    async fn claim_retention_lease(&self, owner: &str, ttl: Duration) -> Result<bool, EngineError> {
        // An upsert rather than an update, so a missing lease row (a hand
        // truncated table, a restore that dropped it) self-heals instead of
        // silently stopping retention forever — a feature that quietly stops
        // running is the one failure mode nobody notices until the disk does.
        let claimed: Option<String> = sqlx::query_scalar(
            "insert into rbpmn_retention_lease (only_row, owner, until) \
             values (true, $1, now() + make_interval(secs => $2::double precision)) \
             on conflict (only_row) do update \
                 set owner = excluded.owner, until = excluded.until \
                 where rbpmn_retention_lease.until is null \
                    or rbpmn_retention_lease.until < now() \
             returning owner",
        )
        .bind(owner)
        .bind(ttl.as_secs_f64())
        .fetch_optional(self.pool())
        .await?;
        Ok(claimed.is_some())
    }

    async fn release_retention_lease(&self, owner: &str) -> Result<(), EngineError> {
        sqlx::query("update rbpmn_retention_lease set owner = null, until = null where owner = $1")
            .bind(owner)
            .execute(self.pool())
            .await?;
        Ok(())
    }

    /// Delete one instance outright — the operator's escape hatch, and the
    /// only way a `failed` instance ever goes away. Archives first when a
    /// sink is registered: an escape hatch that silently bypassed the audit
    /// trail would not be one.
    ///
    /// Refuses an active instance; terminate it first.
    pub async fn delete_instance(&self, id: Uuid) -> Result<RetentionReport, EngineError> {
        let mut conn = self.pool().acquire().await?;
        let record = load_record(&mut conn, id)
            .await?
            .ok_or(EngineError::UnknownInstance(id))?;
        if record.status == "active" {
            return Err(EngineError::InstanceStillActive(id));
        }
        drop(conn);

        let batch = ArchiveBatch {
            instances: vec![record],
        };
        self.run_archive(&batch).await?;

        let mut tx = self.pool().begin().await?;
        // Same lock order as every other path: the instance row first.
        let locked: Option<Uuid> =
            sqlx::query_scalar("select id from rbpmn_instance where id = $1 for update")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        let mut report = RetentionReport::default();
        if locked.is_some() {
            let (events, instances) = delete_records(&mut tx, &[id]).await?;
            report.events_deleted = events;
            report.instances_deleted = instances;
        }
        report.floor = read_floor(&mut tx).await?;
        tx.commit().await?;
        Ok(report)
    }

    /// Every deployed definition version with its blocking reason, if any —
    /// the dry run you read before deleting anything.
    pub async fn prunable_definitions(&self) -> Result<Vec<PrunableDefinition>, EngineError> {
        let rows = sqlx::query(
            "select d.id, d.key, d.version, \
                    (select count(*) from rbpmn_instance i where i.definition_id = d.id) \
                        as instances \
             from rbpmn_definition d order by d.key, d.version",
        )
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let instances: i64 = r.get("instances");
                PrunableDefinition {
                    definition_id: r.get("id"),
                    key: r.get("key"),
                    version: r.get("version"),
                    instances,
                    blocked_by: (instances > 0).then(|| {
                        format!(
                            "{instances} instance(s) still reference it (live, terminal or \
                             pruned — each is runtime state or a history record, and both \
                             need the definition to be intelligible)"
                        )
                    }),
                }
            })
            .collect())
    }

    /// Delete one definition version. Never automatic: definitions grow with
    /// deployments, not throughput, so there is no growth to justify the
    /// risk — only the risk of turning an archive into a pile of element
    /// ids. Refuses while anything still references it.
    ///
    /// Checking instance rows is sufficient to prove no events reference it:
    /// history retention deletes an instance's events *with* its row, in one
    /// transaction, so an event never outlives its instance. That invariant
    /// is what keeps this an indexed lookup instead of a scan of the largest
    /// table in the schema (the fsck asserts it).
    pub async fn delete_definition(&self, key: &str, version: i32) -> Result<(), EngineError> {
        let mut tx = self.pool().begin().await?;
        let row = sqlx::query("select id from rbpmn_definition where key = $1 and version = $2")
            .bind(key)
            .bind(version)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            return Err(EngineError::UnknownDefinitionVersion {
                key: key.to_string(),
                version,
            });
        };
        let definition_id: Uuid = row.get("id");

        let instances: i64 =
            sqlx::query_scalar("select count(*) from rbpmn_instance where definition_id = $1")
                .bind(definition_id)
                .fetch_one(&mut *tx)
                .await?;
        if instances > 0 {
            return Err(EngineError::DefinitionInUse {
                key: key.to_string(),
                version,
                reason: format!("{instances} instance(s) still reference it"),
            });
        }

        sqlx::query("delete from rbpmn_definition where id = $1")
            .bind(definition_id)
            .execute(&mut *tx)
            .await?;
        // The policy is keyed by key, so it outlives individual versions —
        // but not the last one. Leaving it would be config pointing at
        // nothing, which a redeploy would resurrect from code anyway.
        sqlx::query(
            "delete from rbpmn_retention_policy p where p.definition_key = $1 \
             and not exists (select 1 from rbpmn_definition d where d.key = $1)",
        )
        .bind(key)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

/// Instances whose children are due for retirement.
async fn plan_runtime(
    conn: &mut PgConnection,
    options: &RetentionOptions,
) -> Result<Vec<Uuid>, EngineError> {
    // `case when p.definition_key is null` and **not** `coalesce`: a null
    // column means *forever*, which is a policy, while a missing row means
    // *no override*. Coalescing conflates them, and the key that asked to
    // keep its history forever silently inherits the sweeper's default —
    // the one way this feature could delete data nobody asked it to.
    sqlx::query_scalar(
        "select i.id from rbpmn_instance i \
         left join rbpmn_retention_policy p on p.definition_key = i.definition_key \
         where i.status in ('completed', 'terminated') \
           and i.pruned_at is null \
           and i.completed_at is not null \
           and (case when p.definition_key is null then $1 \
                     else p.retain_runtime_secs end) is not null \
           and i.completed_at < now() - make_interval(secs => \
                 (case when p.definition_key is null then $1 \
                       else p.retain_runtime_secs end)::double precision) \
         order by i.completed_at limit $2",
    )
    .bind(options.default_policy.runtime_secs())
    .bind(i64::from(options.max_instances))
    .fetch_all(&mut *conn)
    .await
    .map_err(EngineError::from)
}

/// Records due for deletion, materialised whole. Bounded by both instance
/// and event count, and never splitting an instance — an archived record is
/// complete or absent.
async fn plan_history(
    conn: &mut PgConnection,
    options: &RetentionOptions,
) -> Result<ArchiveBatch, EngineError> {
    let candidates: Vec<Uuid> = sqlx::query_scalar(
        "select i.id from rbpmn_instance i \
         left join rbpmn_retention_policy p on p.definition_key = i.definition_key \
         where i.status in ('completed', 'terminated') \
           and i.completed_at is not null \
           and (case when p.definition_key is null then $1 \
                     else p.retain_history_secs end) is not null \
           and i.completed_at < now() - make_interval(secs => \
                 (case when p.definition_key is null then $1 \
                       else p.retain_history_secs end)::double precision) \
         order by i.completed_at limit $2",
    )
    .bind(options.default_policy.history_secs())
    .bind(i64::from(options.max_instances))
    .fetch_all(&mut *conn)
    .await?;
    if candidates.is_empty() {
        return Ok(ArchiveBatch::default());
    }

    // Size the batch before materialising it: an instance with a million
    // events must not be discovered by loading it.
    let counts = sqlx::query(
        "select instance_id, count(*) as n from rbpmn_event \
         where instance_id = any($1) group by instance_id",
    )
    .bind(&candidates)
    .fetch_all(&mut *conn)
    .await?;
    let mut by_instance: std::collections::HashMap<Uuid, i64> = counts
        .into_iter()
        .map(|r| (r.get("instance_id"), r.get("n")))
        .collect();

    let mut chosen = Vec::new();
    let mut budget: i64 = 0;
    for id in candidates {
        let n = by_instance.remove(&id).unwrap_or(0);
        // Always take the first record whole, however large: otherwise a
        // single oversized instance would stall retention forever.
        if !chosen.is_empty() && budget + n > i64::from(options.max_events) {
            break;
        }
        budget += n;
        chosen.push(id);
    }

    let mut instances = Vec::with_capacity(chosen.len());
    for id in chosen {
        if let Some(record) = load_record(&mut *conn, id).await? {
            instances.push(record);
        }
    }
    Ok(ArchiveBatch { instances })
}

/// One instance, header and history, exactly as it will be archived.
async fn load_record(
    conn: &mut PgConnection,
    id: Uuid,
) -> Result<Option<InstanceRecord>, EngineError> {
    let sql = format!(
        "select i.id, i.definition_key, d.version, i.business_key, i.status, i.variables, \
                {}, {}, {} \
         from rbpmn_instance i join rbpmn_definition d on d.id = i.definition_id \
         where i.id = $1",
        ts("i.created_at", "created_at"),
        ts("i.completed_at", "completed_at"),
        ts("i.pruned_at", "pruned_at"),
    );
    let Some(row) = sqlx::query(&sql)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await?
    else {
        return Ok(None);
    };
    Ok(Some(InstanceRecord {
        id: row.get("id"),
        definition_key: row.get("definition_key"),
        definition_version: row.get("version"),
        business_key: row.get("business_key"),
        status: row.get("status"),
        variables: row.get("variables"),
        created_at: row.get("created_at"),
        completed_at: row.get("completed_at"),
        pruned_at: row.get("pruned_at"),
        events: load_history(&mut *conn, id).await?,
    }))
}

/// An instance's events in semantic order — `id`, per the stream contract.
async fn load_history(conn: &mut PgConnection, id: Uuid) -> Result<Vec<EventRecord>, EngineError> {
    let sql = format!(
        "select id, txid::text::bigint as txid, instance_id, definition_key, kind, \
                element_id, payload, {} \
         from rbpmn_event where instance_id = $1 order by id",
        ts("at", "at"),
    );
    let rows = sqlx::query(&sql).bind(id).fetch_all(&mut *conn).await?;
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

/// Retire the runtime children of already-locked instances; the records
/// stay. Deleted explicitly rather than by cascade, because the cascade is
/// reserved for deleting the record itself.
async fn prune_children(conn: &mut PgConnection, ids: &[Uuid]) -> Result<u64, EngineError> {
    for table in [
        "rbpmn_work_item",
        "rbpmn_timer",
        "rbpmn_subscription",
        "rbpmn_token",
        "rbpmn_scope",
    ] {
        sqlx::query(&format!("delete from {table} where instance_id = any($1)"))
            .bind(ids)
            .execute(&mut *conn)
            .await?;
    }
    let pruned = sqlx::query("update rbpmn_instance set pruned_at = now() where id = any($1)")
        .bind(ids)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    Ok(pruned)
}

/// Delete records outright: events and instance rows together, floor
/// advanced to the highest `(txid, id)` actually removed. Returns
/// `(events, instances)`.
async fn delete_records(conn: &mut PgConnection, ids: &[Uuid]) -> Result<(u64, u64), EngineError> {
    // The floor comes from the rows being deleted *now*, not from the plan:
    // `skip locked` may have dropped instances between the two, and a floor
    // above anything actually deleted would truncate readers for nothing.
    let high = sqlx::query(
        "select txid::text::bigint as txid, id from rbpmn_event \
         where instance_id = any($1) order by txid desc, id desc limit 1",
    )
    .bind(ids)
    .fetch_optional(&mut *conn)
    .await?;

    let events = sqlx::query("delete from rbpmn_event where instance_id = any($1)")
        .bind(ids)
        .execute(&mut *conn)
        .await?
        .rows_affected();
    // Children go with it, by cascade.
    let instances = sqlx::query("delete from rbpmn_instance where id = any($1)")
        .bind(ids)
        .execute(&mut *conn)
        .await?
        .rows_affected();

    if let Some(row) = high {
        let txid: i64 = row.get("txid");
        let id: i64 = row.get("id");
        // Lexicographic and monotonic: comparing the pair, never the
        // components separately — a componentwise max could invent a floor
        // higher than anything ever deleted and truncate readers for free.
        sqlx::query(
            "update rbpmn_retention_floor set txid = $1::text::xid8, id = $2 \
             where (txid, id) < ($1::text::xid8, $2)",
        )
        .bind(txid.to_string())
        .bind(id)
        .execute(&mut *conn)
        .await?;
    }
    Ok((events, instances))
}

async fn read_floor(conn: &mut PgConnection) -> Result<EventCursor, EngineError> {
    let row = sqlx::query("select txid::text::bigint as txid, id from rbpmn_retention_floor")
        .fetch_one(&mut *conn)
        .await?;
    Ok(EventCursor {
        txid: row.get("txid"),
        id: row.get("id"),
    })
}
