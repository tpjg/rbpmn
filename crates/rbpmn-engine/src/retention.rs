//! Retention (phase 7): the bounded deletion of history, and nothing else.
//!
//! **A record retires whole.** One age per definition; the instance row, its
//! children and its events go together, in one transaction, after the archive
//! sink (if any) has been handed a complete copy. A two-stage design was
//! built first — retire the children early, keep the record as its own
//! history header — and then collapsed on measurement: a completed instance
//! emits tens of events against a handful of work items, and its tokens,
//! timers, subscriptions and scopes are already gone (completion removes
//! them), so the early stage reclaimed roughly a tenth of the footprint in
//! exchange for a column, a partial index, a guard on the hot step path, a
//! second planner, and a narrowed idempotency contract. Events dominate that
//! arithmetic completely, and long histories belong in the archive rather
//! than in Postgres.
//!
//! Collapsing it also let `rbpmn_event.instance_id` become a real foreign
//! key (`on delete cascade`, migration 0007). "An event never outlives its
//! instance" is no longer an invariant this codebase asserts and tests — it
//! is one the database will not let it break.
//!
//! **Two phases, and the gap between them holds no transaction.** A pass is
//! `plan` → `execute`, with the archive call in between. The sink reaches an
//! object store over the network, and *any* open transaction holds back
//! `pg_snapshot_xmin` — which is cluster-wide, and which is exactly what the
//! event stream's safe horizon is built on. A sink call inside the deletion
//! transaction would stall every tailing consumer in the cluster for the
//! duration of an S3 upload: the feature that archives history would freeze
//! the stream that reads it. Hence the split, and hence the cross-node claim
//! is a *lease row* rather than a session advisory lock (which would leak
//! forever if a pass were cancelled mid-flight) — the same "a lease is a row
//! value, never an open transaction" rule the task API already follows.
//!
//! What makes the gap safe is that **retention only ever selects immutable
//! data**: a terminal instance cannot become non-terminal, and its events are
//! append-only and closed. The planned set cannot change under the archive
//! call, so [`Engine::execute_retention`] needs no reconciliation — only a
//! re-check under the row lock, which it does anyway.
//!
//! Failure modes, stated rather than discovered:
//!
//! - **The sink fails → nothing is deleted.** No archive, no deletion, ever —
//!   on *every* path, because `execute_retention` runs the sink itself rather
//!   than trusting its caller to have done so.
//! - **The sink succeeds, then the process dies → the batch is archived
//!   again next pass.** Export is **at-least-once**; every record carries its
//!   instance id, so an object-per-instance layout makes the retry an
//!   idempotent overwrite. Two sweepers whose leases overlap have the same
//!   effect.
//!
//! **What retention will not do.** It never touches an active instance. It
//! never touches a `failed` one at any age — an incident is frozen evidence
//! and a repair target, and a sweep that tidied it away would destroy both.
//! It never deletes a definition, because an automatic sweep is justified by
//! unbounded growth and by nothing else: definitions grow with deployments,
//! not throughput, and a deleted definition turns an archive into a pile of
//! element ids. Definitions go only through [`Engine::delete_definition`],
//! named one at a time, guarded. And retention writes nothing to the event
//! stream — it would grow what it prunes and inject rows for instances that
//! no longer exist.
//!
//! Because eligibility is evaluated **per instance**, a wedged instance never
//! blocks its neighbours' retirement. The tempting alternative — a global
//! watermark at "the oldest event of any non-terminal instance" — is one
//! number, trivially safe, and permanently jammed by a single stuck instance
//! until the disk fills.

use crate::events::{EventCursor, EventRecord};
use crate::runtime::ts;
use crate::{Engine, EngineError};
use sqlx::{PgConnection, Row};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use uuid::Uuid;

/// A century, which is the ceiling that actually exists. `i64::MAX` was the
/// obvious bound and the wrong one twice over: `Duration::MAX.as_secs() as
/// i64` is `-1`, turning `now() - interval` into a cutoff in the *future*
/// that deletes every terminal record on the next sweep; and
/// `make_interval(secs => ...)` overflows around 5.6e15 seconds, so an age
/// that passed a type-shaped check would make every sweep fail forever
/// behind a single warn line. "Forever" is spelled `None`, never a very
/// large duration.
const MAX_RETAIN_SECS: u64 = 100 * 365 * 24 * 3600;

/// How long a completed record is kept, measured from `completed_at`.
/// Constructed only through [`RetentionPolicy::new`] and
/// [`RetentionPolicy::forever`] — the field is private so the validation
/// cannot be walked around with a struct literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    retain: Option<Duration>,
}

impl RetentionPolicy {
    /// Keep everything, forever. An explicit and entirely valid choice, and
    /// the default for everything: deleting data is opted into, never
    /// inherited.
    pub fn forever() -> Self {
        RetentionPolicy { retain: None }
    }

    /// Keep records for `retain` past completion. `None` means forever.
    pub fn new(retain: Option<Duration>) -> Result<Self, EngineError> {
        if let Some(d) = retain
            && d.as_secs() > MAX_RETAIN_SECS
        {
            return Err(EngineError::InvalidRetentionPolicy(format!(
                "retention age {d:?} is out of range (at most {MAX_RETAIN_SECS} seconds, \
                 about a century) — spell 'keep forever' as None, never as a very large \
                 duration"
            )));
        }
        Ok(RetentionPolicy { retain })
    }

    /// The configured age; `None` is forever.
    pub fn retain(&self) -> Option<Duration> {
        self.retain
    }

    fn secs(&self) -> Option<i64> {
        // Lossless: the constructor refused anything wider.
        self.retain.map(|d| d.as_secs() as i64)
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
    /// Instances considered per pass.
    pub max_instances: u32,
    /// Soft ceiling on events archived+deleted per pass. Never splits an
    /// instance: a record is archived whole or not at all.
    pub max_events: u32,
    /// How long a pass's claim on the sweep is held. A pass that overruns it
    /// may be joined by another node — which costs a duplicate archive
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
    /// The exact definition this ran on. Carried so an archive can be joined
    /// to a separately kept copy of the model: the record holds element ids,
    /// not BPMN, and duplicating the XML into every record would dwarf the
    /// record for a short instance.
    pub definition_id: Uuid,
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

/// What one pass proposes to delete. Produced by [`Engine::plan_retention`],
/// consumed by [`Engine::execute_retention`].
#[derive(Debug, Clone, Default)]
pub struct RetentionBatch {
    records: ArchiveBatch,
    oversized: u64,
}

impl RetentionBatch {
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// The records this pass would delete — what gets archived.
    pub fn records(&self) -> &ArchiveBatch {
        &self.records
    }

    /// Due records skipped because one of them carries more events than
    /// [`RetentionOptions::max_events`] allows a whole batch to hold. Never
    /// silent: each is logged, and this is the number to alarm on.
    pub fn oversized_skipped(&self) -> u64 {
        self.oversized
    }
}

/// What a pass actually did (after row locks and re-checks).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RetentionReport {
    pub instances_deleted: u64,
    pub events_deleted: u64,
    /// Due records this pass refused to carry because a single one exceeds
    /// [`RetentionOptions::max_events`]. They stay until the ceiling is
    /// raised; retention is not stalled by them, but it is not done either.
    pub oversized_skipped: u64,
    /// The truncation floor after the pass.
    pub floor: EventCursor,
}

impl RetentionReport {
    /// Did this pass move anything? Drives the sweeper's sleep, exactly as
    /// `Drain` drives the scheduler's.
    pub fn moved(&self) -> bool {
        self.instances_deleted > 0
    }
}

/// Whether a definition version can be deleted, and if not, what holds it.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PrunableDefinition {
    pub definition_id: Uuid,
    pub key: String,
    pub version: i32,
    /// Instance rows still referencing it. Each is either live runtime state
    /// or a retained history record, and both need the definition to be
    /// intelligible.
    pub instances: i64,
    /// Records retention has already retired. Their history lives in an
    /// archive that references this model's element ids, so they block
    /// deletion just as live rows do.
    pub retired_instances: i64,
    /// `None` when the version is safe to delete.
    pub blocked_by: Option<String>,
}

/// The one eligibility predicate, interpolated by the planner *and* by the
/// deletion's re-check under the row lock, so the two cannot drift.
///
/// **Parameter contract: the default age is `$1`.** It is baked in, so any
/// query interpolating this must bind the sweeper default there and number
/// its own parameters from `$2`. Getting that wrong would silently bind
/// something else as the cutoff, which decides how much history is deleted.
///
/// `case when p.definition_key is null` and **not** `coalesce`: a null column
/// means *forever*, which is a policy, while a missing row means *no
/// override*. Coalescing conflates them, and the key that asked to keep its
/// history forever silently inherits the sweeper's default — the one way this
/// feature could delete data nobody asked it to.
const DUE: &str = "i.status in ('completed', 'terminated') \
     and i.completed_at is not null \
     and (case when p.definition_key is null then $1 else p.retain_secs end) is not null \
     and i.completed_at < now() - make_interval(secs => \
           (case when p.definition_key is null then $1 else p.retain_secs end)::double precision)";

impl Engine {
    /// The current truncation floor: everything at or below it may have been
    /// deleted. A consumer that has been away resumes here after a
    /// [`EngineError::CursorTruncated`].
    pub async fn retention_floor(&self) -> Result<EventCursor, EngineError> {
        let mut conn = self.pool().acquire().await?;
        read_floor(&mut conn).await
    }

    /// Set the policy for one definition key. Keyed by key, not version —
    /// retention is operational, not semantic. Idempotent.
    ///
    /// Refuses a key that has never been deployed. A typo'd key would
    /// otherwise be stored happily while the sweeper's default kept deleting
    /// the very history the call meant to protect — "seems to run", which is
    /// what deploy-time validation exists to prevent everywhere else.
    pub async fn set_retention_policy(
        &self,
        definition_key: &str,
        policy: RetentionPolicy,
    ) -> Result<(), EngineError> {
        crate::runtime::reject_nul_text(definition_key, "definition key")?;
        let known: bool =
            sqlx::query_scalar("select exists (select 1 from rbpmn_definition where key = $1)")
                .bind(definition_key)
                .fetch_one(self.pool())
                .await?;
        if !known {
            return Err(EngineError::UnknownDefinition(definition_key.to_string()));
        }
        sqlx::query(
            "insert into rbpmn_retention_policy (definition_key, retain_secs) \
             values ($1, $2) \
             on conflict (definition_key) do update \
                 set retain_secs = excluded.retain_secs, updated_at = now()",
        )
        .bind(definition_key)
        .bind(policy.secs())
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// The stored override for a key, if any.
    pub async fn retention_policy(
        &self,
        definition_key: &str,
    ) -> Result<Option<RetentionPolicy>, EngineError> {
        let row =
            sqlx::query("select retain_secs from rbpmn_retention_policy where definition_key = $1")
                .bind(definition_key)
                .fetch_optional(self.pool())
                .await?;
        let Some(row) = row else { return Ok(None) };
        let secs: Option<i64> = row.get("retain_secs");
        // A negative age would be a cutoff in the future — "delete
        // everything, now". Refuse it rather than clamp: a stored value this
        // wrong means something bypassed the constructor and the CHECK, and
        // guessing what it meant is exactly the silent reinterpretation this
        // engine refuses.
        if let Some(s) = secs
            && s < 0
        {
            return Err(EngineError::InvalidRetentionPolicy(format!(
                "stored retention age for '{definition_key}' is negative ({s}s)"
            )));
        }
        Ok(Some(RetentionPolicy {
            retain: secs.map(|s| Duration::from_secs(s as u64)),
        }))
    }

    /// Phase one of a pass: read-only, bounded, and — because it only ever
    /// selects immutable data — stable until executed.
    ///
    /// Event bodies are materialised **only when an archive sink is
    /// registered** — without one they would be loaded solely to be deleted,
    /// and that waste is what turned a large record into an out-of-memory.
    /// The set of records is identical either way, and since
    /// [`Engine::execute_retention`] runs the sink itself, registering one is
    /// the only way to archive: there is no path that needs bodies the plan
    /// does not carry. The load is three set-based queries regardless of
    /// batch size, and [`RetentionOptions::max_events`] bounds both the batch
    /// and any single record in it.
    pub async fn plan_retention(
        &self,
        options: &RetentionOptions,
    ) -> Result<RetentionBatch, EngineError> {
        let mut conn = self.pool().acquire().await?;
        let with_events = self.inner_archive().is_some();
        let (records, oversized) = plan_due(&mut conn, options, with_events).await?;
        Ok(RetentionBatch { records, oversized })
    }

    /// Phase two: archive, then the short transaction. Re-checks every
    /// instance under its row lock (`skip locked` — an instance being stepped
    /// right now is left for the next pass, never an error), deletes, and
    /// advances the floor to the highest `(txid, id)` it actually removed.
    ///
    /// Runs the archive sink itself rather than trusting the caller to have
    /// done so: this is a public entry point, and "no archive, no deletion"
    /// has to be a property of the code rather than of the calling
    /// convention. The sink is invoked *before* the transaction opens, so the
    /// no-transaction-across-the-network rule is preserved.
    pub async fn execute_retention(
        &self,
        batch: &RetentionBatch,
        options: &RetentionOptions,
    ) -> Result<RetentionReport, EngineError> {
        self.run_archive(batch.records()).await?;

        let ids: Vec<Uuid> = batch.records.instances.iter().map(|i| i.id).collect();
        let mut tx = self.pool().begin().await?;
        let mut report = RetentionReport::default();
        if !ids.is_empty() {
            // The whole `DUE` predicate is re-applied here, not just the
            // status: the *instances* are immutable across the archive gap
            // but the policy that made them due is not. An operator who
            // notices a mis-set age and calls `set_retention_policy(...,
            // forever())` while a sweep is blocked on an upload must have
            // that stop the in-flight batch too, not only the next one.
            let locked: Vec<Uuid> = sqlx::query_scalar(&format!(
                "select i.id from rbpmn_instance i \
                 left join rbpmn_retention_policy p on p.definition_key = i.definition_key \
                 where i.id = any($2) and {DUE} \
                 for update of i skip locked"
            ))
            .bind(options.default_policy.secs())
            .bind(&ids)
            .fetch_all(&mut *tx)
            .await?;
            if !locked.is_empty() {
                let (events, instances) = delete_records(&mut tx, &locked).await?;
                report.events_deleted = events;
                report.instances_deleted = instances;
            }
        }
        report.oversized_skipped = batch.oversized;
        report.floor = read_floor(&mut tx).await?;
        tx.commit().await?;
        Ok(report)
    }

    /// One complete pass: claim the lease, plan, archive, execute. Returns an
    /// empty report (carrying the real floor) when another node holds the
    /// lease or nothing is due. Deterministic and side-effect-complete — the
    /// tests drive this, and so can a cron job that would rather not run a
    /// daemon.
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
        let report = self.execute_retention(&batch, options).await?;
        tracing::info!(
            deleted = report.instances_deleted,
            events = report.events_deleted,
            oversized = report.oversized_skipped,
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

    /// Hand a batch to the archive sink, if one is registered. Any failure is
    /// fatal to the pass: no archive, no deletion.
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
                    // At *least* the idle interval, never less: a failing
                    // pass still runs a full plan, and retrying it more often
                    // than a healthy idle pass is not what backing off means.
                    options.sweep_interval.max(ERROR_BACKOFF)
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
        // Probe the status *before* materialising anything: loading a large
        // instance's whole history only to refuse it would make the rejection
        // path the expensive one. Terminal states are listed positively, so a
        // status added later is refused rather than silently deletable.
        let status: Option<String> =
            sqlx::query_scalar("select status from rbpmn_instance where id = $1")
                .bind(id)
                .fetch_optional(&mut *conn)
                .await?;
        match status.as_deref() {
            None => return Err(EngineError::UnknownInstance(id)),
            Some("completed") | Some("terminated") | Some("failed") => {}
            Some(_) => return Err(EngineError::InstanceStillActive(id)),
        }
        let record = load_records(&mut conn, &[id], true)
            .await?
            .pop()
            .ok_or(EngineError::UnknownInstance(id))?;
        drop(conn);

        self.run_archive(&ArchiveBatch {
            instances: vec![record],
        })
        .await?;

        let mut tx = self.pool().begin().await?;
        // Same lock order as every other path: the instance row first. It is
        // terminal, so no step path contends for it; at worst a sweep's own
        // short delete transaction holds it for a moment.
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
            "select d.id, d.key, d.version, d.retired_instances, \
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
                let retired: i64 = r.get("retired_instances");
                PrunableDefinition {
                    definition_id: r.get("id"),
                    key: r.get("key"),
                    version: r.get("version"),
                    instances,
                    retired_instances: retired,
                    blocked_by: match (instances, retired) {
                        (0, 0) => None,
                        (n, 0) => Some(format!("{n} instance(s) still reference it")),
                        (0, r) => Some(format!(
                            "retention has retired {r} record(s) of it; their archived history \
                             needs this model to be intelligible"
                        )),
                        (n, r) => Some(format!(
                            "{n} instance(s) still reference it, and retention has retired \
                             {r} more"
                        )),
                    },
                }
            })
            .collect())
    }

    /// Delete one definition version. Never automatic: definitions grow with
    /// deployments, not throughput, so there is no growth to justify the risk
    /// — only the risk of turning an archive into a pile of element ids.
    /// Refuses while anything still references it.
    ///
    /// Checking instance rows is sufficient to prove no *events* reference
    /// it: `rbpmn_event.instance_id` is a foreign key with `on delete
    /// cascade`, so an event cannot outlive its instance. That is what keeps
    /// this an indexed lookup instead of a scan of the largest table.
    ///
    /// Live rows are not the whole story, though, and the gap is subtle:
    /// retention exists to remove instance rows, and an archived record
    /// carries element ids but no BPMN — so retiring a definition's history
    /// is exactly what would make the definition look unreferenced. The
    /// `retired_instances` counter closes it. Definitions are bounded (a few
    /// versions per process, a few KB each), so the answer is to refuse with
    /// a reason rather than to copy the model into every archived record.
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
        let retired: i64 =
            sqlx::query_scalar("select retired_instances from rbpmn_definition where id = $1")
                .bind(definition_id)
                .fetch_one(&mut *tx)
                .await?;
        if retired > 0 {
            return Err(EngineError::DefinitionInUse {
                key: key.to_string(),
                version,
                reason: format!(
                    "retention has retired {retired} record(s) of it — their archived \
                     history references this model's element ids and nothing else \
                     explains them"
                ),
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
        // The compiled-process cache is documented as bounded by deploys.
        // Deleting a definition without evicting it would quietly break that
        // bound: one leaked `ExecutableProcess` per deleted version, forever.
        self.forget_compiled(definition_id);
        Ok(())
    }
}

/// A failing pass waits at least this long, on top of the sweep interval.
const ERROR_BACKOFF: Duration = Duration::from_secs(60);

/// Records due for deletion, materialised whole. Bounded by both instance and
/// event count, and never splitting an instance — an archived record is
/// complete or absent. Three set-based queries, whatever the batch size.
async fn plan_due(
    conn: &mut PgConnection,
    options: &RetentionOptions,
    with_events: bool,
) -> Result<(ArchiveBatch, u64), EngineError> {
    let candidates: Vec<Uuid> = sqlx::query_scalar(&format!(
        "select i.id from rbpmn_instance i \
         left join rbpmn_retention_policy p on p.definition_key = i.definition_key \
         where {DUE} order by i.completed_at limit $2"
    ))
    .bind(options.default_policy.secs())
    .bind(i64::from(options.max_instances))
    .fetch_all(&mut *conn)
    .await?;
    if candidates.is_empty() {
        return Ok((ArchiveBatch::default(), 0));
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

    let ceiling = i64::from(options.max_events);
    let mut chosen = Vec::new();
    let mut oversized = 0u64;
    let mut budget: i64 = 0;
    for id in candidates {
        let n = by_instance.remove(&id).unwrap_or(0);
        // `max_events` is a ceiling on a single record too, not only on the
        // batch. An earlier version always took the first candidate whole
        // "so an oversized record cannot stall retention" — which achieved
        // the opposite: the oversized record is by definition the *oldest*
        // candidate, so every pass would load its whole history into memory,
        // die, and retry it forever. Skipping it loudly keeps the rest
        // retiring, which is the same rule a wedged instance already gets.
        if n > ceiling {
            oversized += 1;
            tracing::warn!(
                instance = %id,
                events = n,
                max_events = ceiling,
                "instance has more events than one retention batch may carry; \
                 skipping it — raise RetentionOptions::max_events to retire it"
            );
            continue;
        }
        if !chosen.is_empty() && budget + n > ceiling {
            break;
        }
        budget += n;
        chosen.push(id);
    }

    Ok((
        ArchiveBatch {
            instances: load_records(&mut *conn, &chosen, with_events).await?,
        },
        oversized,
    ))
}

/// Headers and histories for a set of instances, in two queries rather than
/// two per instance. Returned in the order given.
///
/// `with_events` is false when no archive sink is registered: the bodies
/// would be loaded solely to be deleted, and that waste is what turns a large
/// record into an out-of-memory instead of a slow query.
async fn load_records(
    conn: &mut PgConnection,
    ids: &[Uuid],
    with_events: bool,
) -> Result<Vec<InstanceRecord>, EngineError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let headers = sqlx::query(&format!(
        "select i.id, i.definition_id, i.definition_key, d.version, i.business_key, \
                i.status, i.variables, \
                {}, {} \
         from rbpmn_instance i join rbpmn_definition d on d.id = i.definition_id \
         where i.id = any($1)",
        ts("i.created_at", "created_at"),
        ts("i.completed_at", "completed_at"),
    ))
    .bind(ids)
    .fetch_all(&mut *conn)
    .await?;

    let event_rows = if with_events {
        sqlx::query(&format!(
            "select id, txid::text::bigint as txid, instance_id, definition_key, kind, \
                    element_id, payload, {} \
             from rbpmn_event where instance_id = any($1) order by instance_id, id",
            ts("at", "at"),
        ))
        .bind(ids)
        .fetch_all(&mut *conn)
        .await?
    } else {
        Vec::new()
    };

    // Ascending `id` per instance is the semantic order (the stream
    // contract); the query's ORDER BY already delivers it that way.
    let mut histories: std::collections::HashMap<Uuid, Vec<EventRecord>> =
        std::collections::HashMap::new();
    for r in event_rows {
        let instance_id: Uuid = r.get("instance_id");
        histories.entry(instance_id).or_default().push(EventRecord {
            id: r.get("id"),
            txid: r.get("txid"),
            instance_id,
            definition_key: r.get("definition_key"),
            kind: r.get("kind"),
            element_id: r.get("element_id"),
            payload: r.get("payload"),
            at: r.get("at"),
        });
    }

    let mut by_id: std::collections::HashMap<Uuid, InstanceRecord> = headers
        .into_iter()
        .map(|row| {
            let id: Uuid = row.get("id");
            (
                id,
                InstanceRecord {
                    id,
                    definition_id: row.get("definition_id"),
                    definition_key: row.get("definition_key"),
                    definition_version: row.get("version"),
                    business_key: row.get("business_key"),
                    status: row.get("status"),
                    variables: row.get("variables"),
                    created_at: row.get("created_at"),
                    completed_at: row.get("completed_at"),
                    events: histories.remove(&id).unwrap_or_default(),
                },
            )
        })
        .collect();
    Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
}

/// Delete records outright, floor advanced to the highest `(txid, id)`
/// actually removed. Returns `(events, instances)`.
async fn delete_records(conn: &mut PgConnection, ids: &[Uuid]) -> Result<(u64, u64), EngineError> {
    // One aggregate, not a max-lookup plus a count: both walked the same rows
    // inside the deletion transaction, which the design wants short. The
    // ordering keys are the *raw* columns — `order by txid desc` on the
    // `txid::text::bigint` output alias would bind to the cast and sort
    // instead of walking `rbpmn_event_stream (txid, id)` backwards.
    //
    // The floor comes from the rows being deleted *now*, not from the plan:
    // `skip locked` may have dropped instances between the two, and a floor
    // above anything actually deleted would truncate readers for nothing.
    let summary = sqlx::query(
        "select count(*) as n, \
                (select txid::text::bigint from rbpmn_event \
                 where instance_id = any($1) order by txid desc, id desc limit 1) as top_txid, \
                (select id from rbpmn_event \
                 where instance_id = any($1) order by txid desc, id desc limit 1) as top_id \
         from rbpmn_event where instance_id = any($1)",
    )
    .bind(ids)
    .fetch_one(&mut *conn)
    .await?;
    let events: i64 = summary.get("n");
    let top: Option<(i64, i64)> = summary
        .get::<Option<i64>, _>("top_txid")
        .zip(summary.get::<Option<i64>, _>("top_id"));

    // Retiring a record is the only thing that can make a definition look
    // unreferenced while its history lives on in someone's archive. Counting
    // it here is what lets `delete_definition` refuse with a reason instead
    // of succeeding because retention did its job.
    sqlx::query(
        "update rbpmn_definition d set retired_instances = d.retired_instances + x.n \
         from (select definition_id, count(*) as n from rbpmn_instance \
               where id = any($1) group by definition_id) x \
         where d.id = x.definition_id",
    )
    .bind(ids)
    .execute(&mut *conn)
    .await?;

    // Events go by cascade from the instance row — the foreign key added in
    // 0007 is what makes that both automatic and impossible to get wrong.
    let instances = sqlx::query("delete from rbpmn_instance where id = any($1)")
        .bind(ids)
        .execute(&mut *conn)
        .await?
        .rows_affected();

    if let Some((txid, id)) = top {
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
    Ok((events.max(0) as u64, instances))
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
