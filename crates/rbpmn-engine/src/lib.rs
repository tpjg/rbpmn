//! rbpmn-engine: the PostgreSQL projection of the pure semantic core.
//!
//! The engine advances tokens **inside a database transaction**: load the
//! affected instance's rows, rebuild the quiescent core state, run the pure
//! `step`, write rows + events, commit. Wait states are the transaction
//! boundaries — which is why process transitions can share a transaction
//! with business writes (a property remote engines cannot offer).
//!
//! The **environment** (handlers, declared pull-worker topics) grows
//! monotonically: register more at any time; a deploy validates against the
//! environment as it exists at that moment (`unresolved-topic`). Every
//! registration and every deploy is idempotent — safely retryable
//! infrastructure. Rows are the runtime truth; the core state is rebuilt
//! from them on every step (`InstanceState::rehydrate`).

#![forbid(unsafe_code)]

mod deploy;
mod error;
mod events;
#[cfg(feature = "http")]
mod http_handler;
mod inspect;
mod listen;
mod retention;
mod runtime;
mod scheduler;
mod tasks;
#[cfg(feature = "test-util")]
pub mod testing;
mod worker;

pub use error::{
    Completion, Correlation, DeployError, Deployment, EngineError, FailOutcome, StartedInstance,
};
pub use events::{EventCursor, EventRecord};
#[cfg(feature = "http")]
pub use http_handler::HttpPostHandler;
pub use inspect::{
    EventView, InstanceInspection, ScopeView, SubscriptionView, TimerView, TokenView, WorkItemView,
};
pub use rbpmn_core::{Bindings, Event};
pub use retention::{
    ArchiveBatch, ArchiveError, InstanceRecord, PrunableDefinition, RetentionArchive,
    RetentionBatch, RetentionOptions, RetentionPolicy, RetentionReport,
};
pub use runtime::FailOptions;
pub use scheduler::SchedulerOptions;
pub use sqlx::PgPool;
pub use tasks::{
    GetTaskOptions, LockExtension, LockedTask, TaskFilter, TaskOrder, declared_index_name,
};
pub use worker::WorkerOptions;

/// Convenience: connect a pool for the engine (the URL comes from operator
/// config — never from request data).
pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    PgPool::connect(database_url).await
}

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

/// Embedded schema migrations, applied in order inside one transaction.
const MIGRATIONS: &[(i64, &str, &str)] = &[
    (1, "runtime", include_str!("../migrations/0001_runtime.sql")),
    (
        2,
        "environment",
        include_str!("../migrations/0002_environment.sql"),
    ),
    (
        3,
        "timers_messages",
        include_str!("../migrations/0003_timers_messages.sql"),
    ),
    (
        4,
        "task_api",
        include_str!("../migrations/0004_task_api.sql"),
    ),
    (
        5,
        "event_stream",
        include_str!("../migrations/0005_event_stream.sql"),
    ),
    (
        6,
        "subprocess_scopes",
        include_str!("../migrations/0006_subprocess_scopes.sql"),
    ),
    (
        7,
        "retention",
        include_str!("../migrations/0007_retention.sql"),
    ),
];

/// A claimed unit of service work, as handed to a push-mode handler.
#[derive(Debug, Clone)]
pub struct WorkItem {
    pub id: uuid::Uuid,
    pub instance_id: uuid::Uuid,
    pub definition_key: String,
    pub element_id: String,
    pub topic: String,
    pub variables: serde_json::Value,
}

/// A handler failure; `code` feeds error-boundary matching once the retry
/// budget is exhausted (no code, or no matching boundary, means incident).
#[derive(Debug, Clone)]
pub struct HandlerFailure {
    pub code: Option<String>,
    pub message: String,
}

/// Push-mode service task handler, driven by the worker loop. Delivery is
/// at-least-once; the engine guarantees exactly-once *state transition* —
/// handlers must be idempotent. The returned value is an RFC 7386 merge
/// patch applied to the instance variables.
pub trait ServiceTaskHandler: Send + Sync {
    fn execute(
        &self,
        item: WorkItem,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, HandlerFailure>> + Send + '_>>;
}

#[derive(Default)]
struct Environment {
    handlers: BTreeMap<String, Arc<dyn ServiceTaskHandler>>,
    /// Not a redundant mirror of `rbpmn_environment_topic`: this set serves
    /// the bootstrap window (builder declarations before `migrate`/
    /// `sync_environment` have run, when the table may not even exist).
    /// After sync, the DB is a superset and `covered_topics` unions both.
    declared: BTreeSet<String>,
}

pub struct EngineBuilder {
    pool: PgPool,
    env: Environment,
    retry_backoff: std::time::Duration,
    archive: Option<Arc<dyn retention::RetentionArchive>>,
}

impl EngineBuilder {
    pub fn handler(
        mut self,
        topic: impl Into<String>,
        handler: Arc<dyn ServiceTaskHandler>,
    ) -> Self {
        self.env.handlers.insert(topic.into(), handler);
        self
    }

    pub fn declare_topic(mut self, topic: impl Into<String>) -> Self {
        self.env.declared.insert(topic.into());
        self
    }

    /// Base retry backoff (default 5s). A failed item becomes claimable
    /// again after `base * 3^failures` — transient outages are ridden out
    /// instead of burning the whole retry budget in milliseconds.
    pub fn retry_backoff(mut self, base: std::time::Duration) -> Self {
        self.retry_backoff = base;
        self
    }

    /// Where records are written before retention deletes them. Every
    /// deletion path consults it — the sweeper and the explicit
    /// [`Engine::delete_instance`] alike — and a sink failure means nothing
    /// is deleted.
    pub fn archive(mut self, archive: Arc<dyn retention::RetentionArchive>) -> Self {
        self.archive = Some(archive);
        self
    }

    pub fn build(self) -> Engine {
        Engine {
            inner: Arc::new(Inner {
                pool: self.pool,
                env: RwLock::new(self.env),
                retry_backoff: self.retry_backoff,
                deferrals: std::sync::Mutex::new(BTreeMap::new()),
                compiled: RwLock::new(BTreeMap::new()),
                archive: RwLock::new(self.archive),
            }),
        }
    }
}

struct Inner {
    pool: PgPool,
    env: RwLock<Environment>,
    retry_backoff: std::time::Duration,
    /// Instances the scheduler is currently holding off on — see
    /// [`Deferral`]. Shared by the candidate query and the sleep
    /// computation, which is what keeps the two in agreement.
    deferrals: std::sync::Mutex<BTreeMap<uuid::Uuid, Deferral>>,
    /// Compiled definitions by definition id — immutable rows, cached
    /// forever; definitions are few, growth is bounded by deploys.
    compiled: RwLock<BTreeMap<uuid::Uuid, Arc<rbpmn_core::ExecutableProcess>>>,
    /// Where records go before retention deletes them. On the engine rather
    /// than in `RetentionOptions` so that *every* deletion path passes
    /// through it — an escape hatch that could bypass the audit trail would
    /// not be one.
    archive: RwLock<Option<Arc<dyn retention::RetentionArchive>>>,
}

/// The one claimability predicate, shared verbatim by the push worker's
/// claim, the pull API's claim, and the dashboard count — aliases `w` (work
/// item) and `i` (instance) are part of the contract. Three queries, one
/// truth: a claimability change edited here cannot desynchronize them.
pub(crate) const CLAIMABLE: &str = "(w.state = 'available' \
     or (w.state = 'locked' and w.lock_until < now())) \
     and (w.retry_at is null or w.retry_at <= now()) and i.status = 'active'";

/// Why (and until when) the scheduler is skipping an instance. Two very
/// different situations share one map because both must be excluded from
/// the candidate query *and* the sleep computation — a skip reason known to
/// one but not the other is exactly the disagreement that produced this
/// file's recurring busy-spin bugs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Deferral {
    until: std::time::Instant,
    /// Transient unavailability (a peer replica holds the advisory lock, a
    /// caller holds the instance row). Escalates gently: a long-held lock
    /// should cost a handful of wake-ups, not thousands.
    skips: u32,
    /// Hard failures (rehydration/step errors). Escalates steeply and is
    /// what the persisted `timer-fire-failed` events count.
    failures: u32,
}

/// Transient skip backoff: `200ms * 2^(skips-1)`, capped.
const SKIP_BACKOFF: std::time::Duration = std::time::Duration::from_millis(200);
const SKIP_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(5);
/// Hard-failure backoff: `30s * 2^(failures-1)`, capped at an hour. In
/// process memory only: another replica (or a restart) retries sooner,
/// which is exactly the right failover.
const FAILURE_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);
const FAILURE_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(3600);
/// How long a lapsed deferral's history is remembered. Bounds the map while
/// letting consecutive failures escalate across their own backoff windows.
const DEFERRAL_FORGET: std::time::Duration = std::time::Duration::from_secs(3600);
/// Failures whose `timer-fire-failed` event is persisted. Beyond this the
/// backoff keeps growing but the stream stops being flooded by one stuck
/// instance (the warning log continues).
pub(crate) const MAX_RECORDED_TIMER_FAILURES: u32 = 5;

/// `base * 2^(n-1)`, saturating at `max`.
fn backoff(base: std::time::Duration, max: std::time::Duration, n: u32) -> std::time::Duration {
    base.saturating_mul(1u32.checked_shl(n.saturating_sub(1)).unwrap_or(u32::MAX))
        .min(max)
}

#[derive(Clone)]
pub struct Engine {
    inner: Arc<Inner>,
}

impl Engine {
    pub fn builder(pool: PgPool) -> EngineBuilder {
        EngineBuilder {
            pool,
            env: Environment::default(),
            retry_backoff: std::time::Duration::from_secs(5),
            archive: None,
        }
    }

    /// Run the schema migrations. Idempotent; call at startup.
    ///
    /// Hand-rolled on purpose: sqlx's migrator hardcodes its
    /// `_sqlx_migrations` ledger, which would collide with a host
    /// application running its own sqlx migrations in the shared schema.
    /// Every rbpmn relation — this ledger included — is `rbpmn_`-prefixed.
    pub async fn migrate(&self) -> Result<(), EngineError> {
        use sha2::{Digest, Sha256};
        let mut tx = self.inner.pool.begin().await?;
        // Serialize concurrent migrators (replicas booting together).
        sqlx::query("select pg_advisory_xact_lock(hashtext('rbpmn_migrations'))")
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "create table if not exists rbpmn_migrations (\
             version bigint primary key, \
             description text not null, \
             checksum text not null, \
             applied_at timestamptz not null default now())",
        )
        .execute(&mut *tx)
        .await?;
        for &(version, description, sql) in MIGRATIONS {
            let checksum = format!("{:x}", Sha256::digest(sql.as_bytes()));
            let applied: Option<String> =
                sqlx::query_scalar("select checksum from rbpmn_migrations where version = $1")
                    .bind(version)
                    .fetch_optional(&mut *tx)
                    .await?;
            match applied {
                Some(existing) if existing == checksum => continue,
                Some(_) => return Err(EngineError::MigrationDrift(version, description)),
                None => {
                    sqlx::raw_sql(sql).execute(&mut *tx).await?;
                    sqlx::query(
                        "insert into rbpmn_migrations (version, description, checksum) \
                         values ($1, $2, $3)",
                    )
                    .bind(version)
                    .bind(description)
                    .bind(&checksum)
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }
        tx.commit().await?;
        Ok(())
    }

    /// Announce that out-of-process workers poll this topic. Idempotent;
    /// callable at any time **after `migrate`** (the declaration is
    /// persisted to `rbpmn_environment_topic`, so a restart or a replica
    /// resumes the same environment — and so the table must exist; for
    /// pre-migrate wiring use `EngineBuilder::declare_topic` +
    /// `sync_environment`). The environment grows freely; the one guarded
    /// inverse is [`Engine::undeclare_topic`].
    pub async fn declare_topic(&self, topic: impl Into<String>) -> Result<(), EngineError> {
        let topic = topic.into();
        crate::runtime::reject_nul_text(&topic, "topic name")?;
        sqlx::query(
            "insert into rbpmn_environment_topic (name) values ($1) on conflict do nothing",
        )
        .bind(&topic)
        .execute(&self.inner.pool)
        .await?;
        self.inner.env.write().unwrap().declared.insert(topic);
        Ok(())
    }

    /// Withdraw a persisted topic declaration — the inverse of
    /// [`Engine::declare_topic`], with a protection: a topic still needed by
    /// a **relevant** definition (the latest version of any key, or any
    /// version with active instances — the same set startup re-validation
    /// checks) is refused with [`EngineError::TopicInUse`], and so is one
    /// whose definitions cannot be inspected — we only undeclare what we
    /// can *prove* unneeded. Registered handlers deliberately do not count
    /// as a substitute: they are process-local and ephemeral, and a replica
    /// without that handler code would refuse to boot.
    ///
    /// Known limits, by design: a topic still named in `RBPMN_TOPICS` (or a
    /// builder `declare_topic`) returns at the next startup via
    /// `sync_environment` — remove it from config too; and other replicas
    /// keep the topic in their in-memory set until they restart. Absent
    /// topics undeclare as a no-op (idempotent, like everything else).
    pub async fn undeclare_topic(&self, topic: &str) -> Result<(), EngineError> {
        crate::runtime::reject_nul_text(topic, "topic name")?;
        let mut tx = self.inner.pool.begin().await?;
        let rows = sqlx::query(
            "select distinct d.key, d.version, d.bpmn_xml, d.bindings from rbpmn_definition d \
             where d.id in (select definition_id from rbpmn_instance where status = 'active') \
                or (d.key, d.version) in \
                   (select key, max(version) from rbpmn_definition group by key) \
             order by d.key, d.version",
        )
        .fetch_all(&mut *tx)
        .await?;
        let mut needed_by: Vec<String> = Vec::new();
        for row in &rows {
            let key: String = sqlx::Row::get(row, "key");
            let version: i32 = sqlx::Row::get(row, "version");
            match crate::runtime::compile_row(row, &key) {
                Ok(proc) => {
                    if proc.service_topics().any(|(_, t)| t == topic) {
                        needed_by.push(format!("{key} v{version}"));
                    }
                }
                Err(e) => {
                    // Cannot inspect -> cannot prove the topic unneeded ->
                    // refuse loudly rather than guess.
                    needed_by.push(format!("{key} v{version} (uninspectable: {e})"));
                }
            }
        }
        if !needed_by.is_empty() {
            return Err(EngineError::TopicInUse {
                topic: topic.to_string(),
                definitions: needed_by,
            });
        }
        sqlx::query("delete from rbpmn_environment_topic where name = $1")
            .bind(topic)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.inner.env.write().unwrap().declared.remove(topic);
        Ok(())
    }

    /// Converges code/config declarations with the persisted set: pushes
    /// builder-declared topics into the DB and pulls previously persisted
    /// ones into memory. Call at startup after `migrate`.
    pub async fn sync_environment(&self) -> Result<(), EngineError> {
        let declared: Vec<String> = {
            let env = self.inner.env.read().unwrap();
            env.declared.iter().cloned().collect()
        };
        for topic in declared {
            self.declare_topic(topic).await?;
        }
        let rows: Vec<String> = sqlx::query_scalar("select name from rbpmn_environment_topic")
            .fetch_all(&self.inner.pool)
            .await?;
        self.inner.env.write().unwrap().declared.extend(rows);
        Ok(())
    }

    /// Register (or replace) the retention archive sink at any time — the
    /// runtime twin of [`EngineBuilder::archive`].
    pub fn register_archive(&self, archive: Arc<dyn retention::RetentionArchive>) {
        *self.inner.archive.write().unwrap() = Some(archive);
    }

    pub(crate) fn inner_archive(&self) -> Option<Arc<dyn retention::RetentionArchive>> {
        self.inner.archive.read().unwrap().clone()
    }

    /// Evict a compiled definition. The cache is documented as bounded by
    /// deploys; once definitions can be deleted, that bound holds only if
    /// deletion evicts.
    pub(crate) fn forget_compiled(&self, definition_id: uuid::Uuid) {
        self.inner.compiled.write().unwrap().remove(&definition_id);
    }

    /// Register (or re-register: latest binding wins) a push-mode handler.
    /// Idempotent; callable at any time.
    pub fn register_handler(&self, topic: impl Into<String>, handler: Arc<dyn ServiceTaskHandler>) {
        self.inner
            .env
            .write()
            .unwrap()
            .handlers
            .insert(topic.into(), handler);
    }

    pub(crate) fn handled_topics(&self) -> Vec<String> {
        self.inner
            .env
            .read()
            .unwrap()
            .handlers
            .keys()
            .cloned()
            .collect()
    }

    pub(crate) fn handler_for(&self, topic: &str) -> Option<Arc<dyn ServiceTaskHandler>> {
        self.inner.env.read().unwrap().handlers.get(topic).cloned()
    }

    /// Topics the environment covers right now: registered handlers plus
    /// declared topics from memory *and* the persisted set (so replicas see
    /// each other's API declarations without a restart).
    ///
    /// Public because it is the whole of the `unresolved-topic` check that
    /// needs a database: a caller holding this set and a compiled model's
    /// `service_topics()` can reach deploy's verdict itself. That is what
    /// lets the editor validate wiring without uploading the model.
    pub async fn covered_topics(&self) -> Result<BTreeSet<String>, sqlx::Error> {
        let mut covered: BTreeSet<String> = {
            let env = self.inner.env.read().unwrap();
            env.handlers
                .keys()
                .chain(env.declared.iter())
                .cloned()
                .collect()
        };
        let rows: Vec<String> = sqlx::query_scalar("select name from rbpmn_environment_topic")
            .fetch_all(&self.inner.pool)
            .await?;
        covered.extend(rows);
        Ok(covered)
    }

    pub(crate) fn retry_backoff(&self) -> std::time::Duration {
        self.inner.retry_backoff
    }

    pub(crate) fn cached_process(
        &self,
        definition_id: uuid::Uuid,
    ) -> Option<Arc<rbpmn_core::ExecutableProcess>> {
        self.inner
            .compiled
            .read()
            .unwrap()
            .get(&definition_id)
            .cloned()
    }

    pub(crate) fn cache_process(
        &self,
        definition_id: uuid::Uuid,
        proc: Arc<rbpmn_core::ExecutableProcess>,
    ) {
        self.inner
            .compiled
            .write()
            .unwrap()
            .insert(definition_id, proc);
    }

    /// Currently backed-off instances plus the time until the earliest
    /// backoff expires — the scheduler excludes the former from its queries
    /// and bounds its sleep by the latter.
    /// Instances currently deferred, plus the time until the earliest one
    /// becomes eligible again. Both the candidate query and the sleep
    /// computation read this, so they cannot disagree about what is
    /// actionable.
    pub(crate) fn deferral_snapshot(&self) -> (Vec<uuid::Uuid>, Option<std::time::Duration>) {
        let mut map = self.inner.deferrals.lock().unwrap();
        let now = std::time::Instant::now();
        // Expired entries are kept (only their *deferral* lapsed, not their
        // history) and forgotten later: dropping them at expiry would reset
        // the consecutive-failure count on every cycle, so the backoff could
        // never escalate and every retry would re-record the same failure.
        // Real progress calls `clear_deferral`, which is the honest reset.
        map.retain(|_, d| d.until + DEFERRAL_FORGET > now);
        let deferred: Vec<uuid::Uuid> = map
            .iter()
            .filter(|(_, d)| d.until > now)
            .map(|(id, _)| *id)
            .collect();
        let earliest = map
            .values()
            .map(|d| d.until)
            .filter(|until| *until > now)
            .min()
            .map(|until| until - now);
        (deferred, earliest)
    }

    /// A transient skip: someone else is working on this instance, or a
    /// caller holds its row. Escalates so a long-held lock costs few
    /// wake-ups.
    pub(crate) fn defer_transient(&self, instance: uuid::Uuid) {
        let mut map = self.inner.deferrals.lock().unwrap();
        let entry = map.entry(instance).or_insert(Deferral {
            until: std::time::Instant::now(),
            skips: 0,
            failures: 0,
        });
        entry.skips = entry.skips.saturating_add(1);
        entry.until =
            std::time::Instant::now() + backoff(SKIP_BACKOFF, SKIP_BACKOFF_MAX, entry.skips);
    }

    /// A hard failure. Returns the consecutive-failure count, which decides
    /// whether the failure is also persisted as an event.
    pub(crate) fn defer_failed(&self, instance: uuid::Uuid) -> u32 {
        let mut map = self.inner.deferrals.lock().unwrap();
        let entry = map.entry(instance).or_insert(Deferral {
            until: std::time::Instant::now(),
            skips: 0,
            failures: 0,
        });
        entry.failures = entry.failures.saturating_add(1);
        entry.until = std::time::Instant::now()
            + backoff(FAILURE_BACKOFF, FAILURE_BACKOFF_MAX, entry.failures);
        entry.failures
    }

    /// Progress on an instance clears its history: the next hiccup starts
    /// from the shortest backoff again.
    pub(crate) fn clear_deferral(&self, instance: uuid::Uuid) {
        self.inner.deferrals.lock().unwrap().remove(&instance);
    }

    /// Test-only: make a deferred instance eligible again *without*
    /// forgetting why it was deferred — the escalating backoff would
    /// otherwise make failure cycles take minutes to observe. Distinct from
    /// [`Engine::clear_deferral`], which is what real progress does and
    /// deliberately resets the failure history.
    #[cfg(feature = "test-util")]
    pub fn expire_deferral_for_test(&self, instance: uuid::Uuid) {
        if let Some(entry) = self.inner.deferrals.lock().unwrap().get_mut(&instance) {
            entry.until = std::time::Instant::now();
        }
    }

    fn pool(&self) -> &PgPool {
        &self.inner.pool
    }
}
