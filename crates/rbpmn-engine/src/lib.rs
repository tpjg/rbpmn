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
#[cfg(feature = "http")]
mod http_handler;
mod inspect;
mod runtime;
mod scheduler;
#[cfg(feature = "test-util")]
pub mod testing;
mod worker;

pub use error::{
    Completion, Correlation, DeployError, Deployment, EngineError, FailOutcome, StartedInstance,
};
#[cfg(feature = "http")]
pub use http_handler::HttpPostHandler;
pub use inspect::{
    EventView, InstanceInspection, SubscriptionView, TimerView, TokenView, WorkItemView,
};
pub use rbpmn_core::{Bindings, Event};
pub use runtime::FailOptions;
pub use scheduler::SchedulerOptions;
pub use sqlx::PgPool;
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
            }),
        }
    }
}

struct Inner {
    pool: PgPool,
    env: RwLock<Environment>,
    retry_backoff: std::time::Duration,
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
    /// callable at any time — the environment grows monotonically. The
    /// declaration is **persisted**: the deploys it unblocks persist, so a
    /// restart or a replica resumes the same environment.
    pub async fn declare_topic(&self, topic: impl Into<String>) -> Result<(), sqlx::Error> {
        let topic = topic.into();
        sqlx::query(
            "insert into rbpmn_environment_topic (name) values ($1) on conflict do nothing",
        )
        .bind(&topic)
        .execute(&self.inner.pool)
        .await?;
        self.inner.env.write().unwrap().declared.insert(topic);
        Ok(())
    }

    /// Converges code/config declarations with the persisted set: pushes
    /// builder-declared topics into the DB and pulls previously persisted
    /// ones into memory. Call at startup after `migrate`.
    pub async fn sync_environment(&self) -> Result<(), sqlx::Error> {
        let declared: Vec<String> = {
            let env = self.inner.env.read().unwrap();
            env.declared.iter().cloned().collect()
        };
        for topic in declared {
            sqlx::query(
                "insert into rbpmn_environment_topic (name) values ($1) on conflict do nothing",
            )
            .bind(&topic)
            .execute(&self.inner.pool)
            .await?;
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

    fn pool(&self) -> &PgPool {
        &self.inner.pool
    }
}
