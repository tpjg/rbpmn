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
    EventView, InstanceInspection, SubscriptionView, TimerView, TokenView, WorkItemView,
};
pub use rbpmn_core::{Bindings, Event};
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

    pub fn build(self) -> Engine {
        Engine {
            inner: Arc::new(Inner {
                pool: self.pool,
                env: RwLock::new(self.env),
                retry_backoff: self.retry_backoff,
                timer_error_backoff: std::sync::Mutex::new(BTreeMap::new()),
                compiled: RwLock::new(BTreeMap::new()),
            }),
        }
    }
}

struct Inner {
    pool: PgPool,
    env: RwLock<Environment>,
    retry_backoff: std::time::Duration,
    /// Instances whose timer firing recently errored, backed off so one
    /// poisoned instance cannot head-of-line-block the scheduler.
    timer_error_backoff: std::sync::Mutex<BTreeMap<uuid::Uuid, std::time::Instant>>,
    /// Compiled definitions by definition id — immutable rows, cached
    /// forever; definitions are few, growth is bounded by deploys.
    compiled: RwLock<BTreeMap<uuid::Uuid, Arc<rbpmn_core::ExecutableProcess>>>,
}

/// The one claimability predicate, shared verbatim by the push worker's
/// claim, the pull API's claim, and the dashboard count — aliases `w` (work
/// item) and `i` (instance) are part of the contract. Three queries, one
/// truth: a claimability change edited here cannot desynchronize them.
pub(crate) const CLAIMABLE: &str = "(w.state = 'available' \
     or (w.state = 'locked' and w.lock_until < now())) \
     and (w.retry_at is null or w.retry_at <= now()) and i.status = 'active'";

/// How long a timer-erroring instance is skipped by the scheduler before
/// being retried. In-process only: another replica (or a restart) retries
/// sooner, which is exactly the right failover.
const TIMER_ERROR_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);

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
    pub(crate) async fn covered_topics(&self) -> Result<BTreeSet<String>, sqlx::Error> {
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
    pub(crate) fn timer_backoff_snapshot(&self) -> (Vec<uuid::Uuid>, Option<std::time::Duration>) {
        let mut map = self.inner.timer_error_backoff.lock().unwrap();
        let now = std::time::Instant::now();
        map.retain(|_, until| *until > now);
        let earliest = map.values().min().map(|until| *until - now);
        (map.keys().copied().collect(), earliest)
    }

    pub(crate) fn set_timer_error_backoff(&self, instance: uuid::Uuid) {
        self.inner
            .timer_error_backoff
            .lock()
            .unwrap()
            .insert(instance, std::time::Instant::now() + TIMER_ERROR_BACKOFF);
    }

    fn pool(&self) -> &PgPool {
        &self.inner.pool
    }
}
