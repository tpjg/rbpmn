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
#[cfg(feature = "test-util")]
pub mod testing;
mod worker;

pub use error::{Completion, DeployError, Deployment, EngineError, FailOutcome, StartedInstance};
#[cfg(feature = "http")]
pub use http_handler::HttpPostHandler;
pub use inspect::{EventView, InstanceInspection, TokenView, WorkItemView};
pub use rbpmn_core::{Bindings, Event};
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
