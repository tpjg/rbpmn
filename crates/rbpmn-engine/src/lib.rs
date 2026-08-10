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
mod runtime;

pub use error::{Completion, DeployError, Deployment, EngineError, FailOutcome, StartedInstance};
pub use rbpmn_core::{Bindings, Event};

use sqlx::PgPool;
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

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

/// Push-mode service task handler. Delivery is at-least-once; the engine
/// guarantees exactly-once *state transition* — handlers must be idempotent.
/// (The worker loop that invokes these lands in the phase-2 follow-up
/// milestone; the registry and the `unresolved-topic` check are live now.)
pub trait ServiceTaskHandler: Send + Sync {
    fn execute(
        &self,
        item: WorkItem,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, String>> + Send + '_>>;
}

#[derive(Default)]
struct Environment {
    handlers: BTreeMap<String, Arc<dyn ServiceTaskHandler>>,
    declared: BTreeSet<String>,
}

pub struct EngineBuilder {
    pool: PgPool,
    env: Environment,
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

    pub fn build(self) -> Engine {
        Engine {
            inner: Arc::new(Inner {
                pool: self.pool,
                env: RwLock::new(self.env),
            }),
        }
    }
}

struct Inner {
    pool: PgPool,
    env: RwLock<Environment>,
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
        }
    }

    /// Run the schema migrations. Idempotent; call at startup.
    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        sqlx::migrate!()
            .run(&self.inner.pool)
            .await
            .map_err(sqlx::Error::from)
    }

    /// Announce that out-of-process workers poll this topic. Idempotent;
    /// callable at any time — the environment grows monotonically.
    pub fn declare_topic(&self, topic: impl Into<String>) {
        self.inner
            .env
            .write()
            .unwrap()
            .declared
            .insert(topic.into());
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

    fn covered_topics(&self) -> BTreeSet<String> {
        let env = self.inner.env.read().unwrap();
        env.handlers
            .keys()
            .chain(env.declared.iter())
            .cloned()
            .collect()
    }

    fn pool(&self) -> &PgPool {
        &self.inner.pool
    }
}
