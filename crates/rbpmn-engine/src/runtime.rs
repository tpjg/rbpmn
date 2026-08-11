//! Transactional stepping: instance rows are locked (`FOR UPDATE`), the core
//! state is rebuilt from rows, the pure step runs, and rows + events are
//! written — all in one transaction. Concurrent completions on one instance
//! serialize on the row lock, which is what makes the parallel join's
//! exactly-once firing hold under concurrency.
//!
//! Every operation exists in two forms: the pool-owning convenience
//! (`start`, `complete_work_item`, `fail_work_item`) that opens and commits
//! its own transaction, and the `*_in_tx` variant taking the **caller's**
//! connection — the design's flagship property that a process transition can
//! share a transaction with business writes. The `_in_tx` forms never
//! commit; the caller does (and must, promptly — the instance row is locked
//! until then). Remote (HTTP) callers cannot share a transaction by nature;
//! their contract is per-call atomicity plus idempotent retries.

use crate::{Completion, Correlation, Engine, EngineError, FailOutcome, StartedInstance};
use rbpmn_core::{
    Bindings, Command, Counters, Event, ExecutableProcess, InstanceState, InstanceStatus,
    SubscriptionId, SubscriptionState, TimerDue, TimerId, TimerState, Token, TokenId, WaitKind,
    WorkItemId, WorkItemState, WorkKind, step,
};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

pub(crate) struct DefinitionRef {
    id: Uuid,
    key: String,
}

/// Options for [`Engine::fail_work_item`].
#[derive(Debug, Clone, Default)]
pub struct FailOptions {
    /// Error code for boundary matching once the retry budget is exhausted.
    pub error_code: Option<String>,
    /// Human-readable failure reason, recorded on the work item
    /// (`last_failure`) and in the retry events — what makes an incident
    /// diagnosable from stored state.
    pub detail: Option<String>,
    /// Lease identity. Failing a *live-leased* item requires its owner;
    /// ownerless calls on a live lease are refused (`ItemLeased`) so an HTTP
    /// fail cannot yank an item out from under a running worker.
    pub owner: Option<String>,
}

impl Engine {
    pub async fn start(
        &self,
        key: &str,
        business_key: Option<&str>,
        variables: serde_json::Value,
    ) -> Result<StartedInstance, EngineError> {
        let mut tx = self.pool().begin().await?;
        let started = self
            .start_in_tx(&mut tx, key, business_key, variables)
            .await?;
        tx.commit().await?;
        Ok(started)
    }

    /// [`Engine::start`] inside the caller's transaction.
    pub async fn start_in_tx(
        &self,
        tx: &mut PgConnection,
        key: &str,
        business_key: Option<&str>,
        variables: serde_json::Value,
    ) -> Result<StartedInstance, EngineError> {
        reject_nul(&variables)?;
        require_object(&variables, "initial variables")?;
        if let Some(bk) = business_key {
            reject_nul_text(bk, "business key")?;
        }
        let definition_id: Uuid = sqlx::query(
            "select id from rbpmn_definition \
             where key = $1 order by version desc limit 1",
        )
        .bind(key)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| EngineError::UnknownDefinition(key.to_string()))?
        .get("id");

        let definition = DefinitionRef {
            id: definition_id,
            key: key.to_string(),
        };
        let proc = compiled_process(self, &mut *tx, definition_id, key).await?;

        let instance_id: Uuid = sqlx::query(
            "insert into rbpmn_instance \
             (definition_id, definition_key, business_key, status, variables) \
             values ($1, $2, $3, 'active', 'null'::jsonb) returning id",
        )
        .bind(definition.id)
        .bind(key)
        .bind(business_key)
        .fetch_one(&mut *tx)
        .await?
        .get("id");

        let mut state = InstanceState::new();
        let events = step(&proc, &mut state, Command::Start { variables })?;
        persist_step(tx, &proc, &definition, instance_id, &state, &events).await?;

        Ok(StartedInstance {
            id: instance_id,
            events,
        })
    }

    /// Ownerless completion: refused while another holder's lease is live
    /// (`ItemLeased`) — a stray call must not discard a working claimant's
    /// result. Claimed tasks are completed through [`Engine::complete_task`]
    /// with the lease identity.
    pub async fn complete_work_item(
        &self,
        work_item: Uuid,
        patch: serde_json::Value,
    ) -> Result<Completion, EngineError> {
        let mut tx = self.pool().begin().await?;
        let completion = self
            .complete_work_item_in_tx(&mut tx, work_item, None, patch)
            .await?;
        tx.commit().await?;
        Ok(completion)
    }

    /// [`Engine::complete_work_item`] inside the caller's transaction, with
    /// the caller's lease identity (`None` = ownerless: only unleased items
    /// may be completed).
    pub async fn complete_work_item_in_tx(
        &self,
        tx: &mut PgConnection,
        work_item: Uuid,
        owner: Option<&str>,
        patch: serde_json::Value,
    ) -> Result<Completion, EngineError> {
        reject_nul(&patch)?;
        require_object(&patch, "completion patch")?;
        let item = sqlx::query("select instance_id, item_no from rbpmn_work_item where id = $1")
            .bind(work_item)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(EngineError::UnknownWorkItem(work_item))?;
        let instance_id: Uuid = item.get("instance_id");
        let item_no: i64 = item.get("item_no");

        // Lock the instance first: every step on an instance serializes
        // here, in the same order engine-wide (instance row, then item row
        // — the one order that can never deadlock the scheduler or a fail).
        let (definition, proc, mut state) = load_instance(self, &mut *tx, instance_id).await?;
        let item_state = guard_lease(&mut *tx, instance_id, item_no, owner, work_item).await?;

        // The idempotent no-op comes before every other gate: a retried,
        // already-committed completion must converge even if a sibling
        // branch has since raised an incident.
        if item_state != "available" && item_state != "locked" {
            return Ok(Completion::AlreadyClosed { state: item_state });
        }
        if state.status == InstanceStatus::Failed {
            return Err(EngineError::IncidentOpen(instance_id));
        }
        if state.status != InstanceStatus::Active {
            return Err(EngineError::InstanceNotActive(
                instance_id,
                status_to_db(state.status).to_string(),
            ));
        }

        let events = step(
            &proc,
            &mut state,
            Command::CompleteWorkItem {
                id: WorkItemId(item_no as u64),
                patch,
            },
        )?;
        persist_step(tx, &proc, &definition, instance_id, &state, &events).await?;
        Ok(Completion::Advanced(events))
    }

    /// Deliver a message to the single open subscription matching
    /// `(message, correlation key value)`, applying the merge patch in the
    /// same step that advances the token. Exactly one subscription must
    /// match: none is [`EngineError::NoSubscription`] (the message has
    /// nowhere to go — loudly, never dropped), several is
    /// [`EngineError::AmbiguousCorrelation`] (delivering to "one of them"
    /// would be a guess).
    pub async fn correlate(
        &self,
        message: &str,
        key: &str,
        patch: serde_json::Value,
    ) -> Result<Correlation, EngineError> {
        let mut tx = self.pool().begin().await?;
        let correlation = self.correlate_in_tx(&mut tx, message, key, patch).await?;
        tx.commit().await?;
        Ok(correlation)
    }

    /// [`Engine::correlate`] inside the caller's transaction.
    pub async fn correlate_in_tx(
        &self,
        tx: &mut PgConnection,
        message: &str,
        key: &str,
        patch: serde_json::Value,
    ) -> Result<Correlation, EngineError> {
        reject_nul(&patch)?;
        require_object(&patch, "message patch")?;
        reject_nul_text(message, "message name")?;
        reject_nul_text(key, "correlation key")?;
        // Resolve without a lock, then lock the instance (the same order as
        // every step path) and re-check the subscription under it. Only
        // *active* instances count: an incident-frozen instance keeps its
        // subscription rows (frozen for repair), and those must not block
        // delivery to a live instance sharing the key — or answer for a key
        // that otherwise has no destination.
        let matches = sqlx::query(
            "select s.instance_id, s.subscription_no from rbpmn_subscription s \
             join rbpmn_instance i on i.id = s.instance_id \
             where s.message_name = $1 and s.correlation_key = $2 \
               and i.status = 'active' limit 2",
        )
        .bind(message)
        .bind(key)
        .fetch_all(&mut *tx)
        .await?;
        let row = match matches.as_slice() {
            [] => {
                return Err(EngineError::NoSubscription {
                    message: message.to_string(),
                    key: key.to_string(),
                });
            }
            [row] => row,
            _ => {
                return Err(EngineError::AmbiguousCorrelation {
                    message: message.to_string(),
                    key: key.to_string(),
                });
            }
        };
        let instance_id: Uuid = row.get("instance_id");
        let subscription_no: i64 = row.get("subscription_no");

        let (definition, proc, mut state) = load_instance(self, &mut *tx, instance_id).await?;
        if state.status == InstanceStatus::Failed {
            return Err(EngineError::IncidentOpen(instance_id));
        }
        if state.status != InstanceStatus::Active {
            return Err(EngineError::InstanceNotActive(
                instance_id,
                status_to_db(state.status).to_string(),
            ));
        }
        // A concurrent step (boundary timer, terminate, another delivery)
        // may have withdrawn it between resolve and lock.
        let sub_id = SubscriptionId(subscription_no as u64);
        if !state.subscriptions().any(|(id, _)| id == sub_id) {
            return Err(EngineError::NoSubscription {
                message: message.to_string(),
                key: key.to_string(),
            });
        }

        let events = step(
            &proc,
            &mut state,
            Command::DeliverMessage { id: sub_id, patch },
        )?;
        persist_step(tx, &proc, &definition, instance_id, &state, &events).await?;
        Ok(Correlation {
            instance_id,
            events,
        })
    }

    /// Handler failure: spend one retry (with exponential backoff before the
    /// item becomes claimable again), or — budget exhausted — raise the named
    /// error into the core: a matching error boundary takes its path; no
    /// match freezes the instance in the incident state.
    pub async fn fail_work_item(
        &self,
        work_item: Uuid,
        options: &FailOptions,
    ) -> Result<FailOutcome, EngineError> {
        let mut tx = self.pool().begin().await?;
        let outcome = self
            .fail_work_item_in_tx(&mut tx, work_item, options)
            .await?;
        tx.commit().await?;
        Ok(outcome)
    }

    /// [`Engine::fail_work_item`] inside the caller's transaction.
    pub async fn fail_work_item_in_tx(
        &self,
        tx: &mut PgConnection,
        work_item: Uuid,
        options: &FailOptions,
    ) -> Result<FailOutcome, EngineError> {
        // Scrubbed, not rejected: a NUL in a handler's failure message must
        // not wedge the fail path itself into an abort loop.
        let options = &FailOptions {
            error_code: options.error_code.as_deref().map(scrub_nul),
            detail: options.detail.as_deref().map(scrub_nul),
            owner: options.owner.clone(),
        };
        let item = sqlx::query("select instance_id, item_no from rbpmn_work_item where id = $1")
            .bind(work_item)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(EngineError::UnknownWorkItem(work_item))?;
        let instance_id: Uuid = item.get("instance_id");
        let item_no: i64 = item.get("item_no");

        let (definition, proc, mut state) = load_instance(self, &mut *tx, instance_id).await?;
        let item_state = guard_lease(
            &mut *tx,
            instance_id,
            item_no,
            options.owner.as_deref(),
            work_item,
        )
        .await?;
        if item_state != "available" && item_state != "locked" {
            // The idempotent no-op, mirroring completion.
            return Ok(FailOutcome::AlreadyClosed { state: item_state });
        }
        if state.status == InstanceStatus::Failed {
            return Err(EngineError::IncidentOpen(instance_id));
        }
        if state.status != InstanceStatus::Active {
            return Err(EngineError::InstanceNotActive(
                instance_id,
                status_to_db(state.status).to_string(),
            ));
        }

        // SET expressions see pre-update values: the backoff exponent uses
        // the failure count before this failure.
        let row = sqlx::query(
            "update rbpmn_work_item set retries = retries - 1, failures = failures + 1, \
             state = 'available', lock_owner = null, lock_until = null, \
             retry_at = now() + make_interval(secs => $3 * power(3, failures)), \
             last_failure = coalesce($4, last_failure) \
             where instance_id = $1 and item_no = $2 \
               and state in ('available', 'locked') \
             returning retries, element_id, topic",
        )
        .bind(instance_id)
        .bind(item_no)
        .bind(self.retry_backoff().as_secs_f64())
        .bind(options.detail.as_deref())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(EngineError::UnknownWorkItem(work_item))?;
        let retries: i32 = row.get("retries");
        let element_id: String = row.get("element_id");
        let topic: String = row.get("topic");

        let outcome = if retries > 0 {
            insert_engine_event(
                &mut *tx,
                &definition,
                instance_id,
                "work-item-retrying",
                &element_id,
                serde_json::json!({
                    "kind": "work-item-retrying",
                    "element": element_id,
                    "retriesLeft": retries,
                    "message": options.detail,
                }),
            )
            .await?;
            notify_work(&mut *tx, &topic).await?;
            FailOutcome::Retrying {
                retries_left: retries,
            }
        } else {
            let events = step(
                &proc,
                &mut state,
                Command::RaiseError {
                    id: WorkItemId(item_no as u64),
                    code: options.error_code.clone(),
                },
            )?;
            persist_step(tx, &proc, &definition, instance_id, &state, &events).await?;
            if events
                .iter()
                .any(|e| matches!(e, Event::IncidentRaised { .. }))
            {
                FailOutcome::IncidentRaised
            } else {
                FailOutcome::ErrorCaught(events)
            }
        };
        Ok(outcome)
    }
}

fn status_to_db(status: InstanceStatus) -> &'static str {
    match status {
        InstanceStatus::Created | InstanceStatus::Active => "active",
        InstanceStatus::Completed => "completed",
        InstanceStatus::Terminated => "terminated",
        InstanceStatus::Failed => "failed",
    }
}

fn status_from_db(status: &str) -> Result<InstanceStatus, EngineError> {
    match status {
        "active" => Ok(InstanceStatus::Active),
        "completed" => Ok(InstanceStatus::Completed),
        "terminated" => Ok(InstanceStatus::Terminated),
        "failed" => Ok(InstanceStatus::Failed),
        other => Err(internal(format!("unknown instance status '{other}'"))),
    }
}

fn work_kind_from_db(kind: &str) -> Result<WorkKind, EngineError> {
    match kind {
        "service" => Ok(WorkKind::Service),
        "user" => Ok(WorkKind::User),
        other => Err(internal(format!("unknown work item kind '{other}'"))),
    }
}

fn internal(message: String) -> EngineError {
    EngineError::Compile(rbpmn_core::CompileError::Internal(message))
}

/// RFC 7386 replaces the whole target when the patch is not an object — for
/// the variables document that means one stray scalar destroys every
/// variable. Reject loudly at the boundary; the same rule guards initial
/// variables (the document must *be* an object).
fn require_object(value: &serde_json::Value, what: &str) -> Result<(), EngineError> {
    if !value.is_object() {
        return Err(EngineError::InvalidVariables(format!(
            "{what} must be a JSON object (a non-object RFC 7386 merge patch \
             would replace the entire variables document)"
        )));
    }
    Ok(())
}

/// Text parameters bound into queries hit the same NUL limitation as jsonb;
/// reject at the boundary so it is a 400, not a transaction-poisoning 500.
pub(crate) fn reject_nul_text(value: &str, what: &str) -> Result<(), EngineError> {
    if value.contains('\u{0}') {
        return Err(EngineError::InvalidVariables(format!(
            "{what} must not contain \\u0000 (PostgreSQL cannot store it)"
        )));
    }
    Ok(())
}

/// Diagnostic text (failure details, error codes) is scrubbed, not rejected:
/// refusing the *fail* path over a NUL byte would loop the failure forever —
/// the exact wedge the fail path exists to resolve.
fn scrub_nul(value: &str) -> String {
    value.replace('\u{0}', "\u{fffd}")
}

/// PostgreSQL jsonb cannot represent NUL in strings; reject it loudly at the
/// boundary instead of poisoning the step transaction forever.
fn reject_nul(value: &serde_json::Value) -> Result<(), EngineError> {
    fn has_nul(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::String(s) => s.contains('\u{0}'),
            serde_json::Value::Array(a) => a.iter().any(has_nul),
            serde_json::Value::Object(m) => {
                m.iter().any(|(k, v)| k.contains('\u{0}') || has_nul(v))
            }
            _ => false,
        }
    }
    if has_nul(value) {
        return Err(EngineError::InvalidVariables(
            "strings must not contain \\u0000 (PostgreSQL jsonb cannot store it)".to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn compile_row(row: &PgRow, key: &str) -> Result<ExecutableProcess, EngineError> {
    // A manifest that stopped deserializing is corruption or an unmigrated
    // schema change — loudly reject, never silently run with empty bindings.
    let bindings: Bindings = serde_json::from_value(row.get::<serde_json::Value, _>("bindings"))
        .map_err(|e| {
            internal(format!(
                "stored bindings manifest does not deserialize: {e}"
            ))
        })?;
    let defs = rbpmn_model::parse(&row.get::<String, _>("bpmn_xml"))
        .map_err(|e| internal(e.to_string()))?;
    Ok(ExecutableProcess::compile(&defs, key, &bindings)?)
}

/// The one lease gate: taken under the instance lock (every caller locks
/// the instance first — a single lock order engine-wide), reading the item
/// row FOR UPDATE so a concurrent claim cannot slip between check and
/// mutation. Live foreign leases are refused; closed items pass through
/// (the caller answers its idempotent AlreadyClosed). Returns the item's
/// state.
pub(crate) async fn guard_lease(
    tx: &mut PgConnection,
    instance_id: Uuid,
    item_no: i64,
    owner: Option<&str>,
    work_item: Uuid,
) -> Result<String, EngineError> {
    let row = sqlx::query(
        "select state, lock_owner, \
         (lock_until is not null and lock_until > now()) as lease_live \
         from rbpmn_work_item where instance_id = $1 and item_no = $2 for update",
    )
    .bind(instance_id)
    .bind(item_no)
    .fetch_one(&mut *tx)
    .await?;
    let state: String = row.get("state");
    if state == "locked"
        && row.get::<bool, _>("lease_live")
        && owner != row.get::<Option<String>, _>("lock_owner").as_deref()
    {
        return Err(EngineError::ItemLeased(work_item));
    }
    Ok(state)
}

/// The definition compile cache: definitions are immutable (insert-only,
/// content-hashed, unique (key, version)), so a compiled process is cached
/// forever by definition id. On a hit the whole-XML fetch and O(model)
/// parse+compile are skipped — this runs inside the held instance lock on
/// every step-like operation, so it is the hottest path there is.
pub(crate) async fn compiled_process(
    engine: &Engine,
    tx: &mut PgConnection,
    definition_id: Uuid,
    key: &str,
) -> Result<std::sync::Arc<ExecutableProcess>, EngineError> {
    if let Some(proc) = engine.cached_process(definition_id) {
        return Ok(proc);
    }
    let row = sqlx::query("select bpmn_xml, bindings from rbpmn_definition where id = $1")
        .bind(definition_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| internal(format!("definition {definition_id} has no row")))?;
    let proc = std::sync::Arc::new(compile_row(&row, key)?);
    engine.cache_process(definition_id, proc.clone());
    Ok(proc)
}

/// Locks the instance row and rebuilds the quiescent core state from rows —
/// rows are the runtime truth, this is their inverse. Every mapping is
/// exhaustive: an unknown status/kind/wait is an error, never a guess.
pub(crate) async fn load_instance(
    engine: &Engine,
    tx: &mut PgConnection,
    instance_id: Uuid,
) -> Result<
    (
        DefinitionRef,
        std::sync::Arc<ExecutableProcess>,
        InstanceState,
    ),
    EngineError,
> {
    let inst = sqlx::query(
        "select i.definition_id, i.definition_key, i.status, i.variables, \
                i.next_token, i.next_work_item, i.next_timer, i.next_subscription \
         from rbpmn_instance i where i.id = $1 for update",
    )
    .bind(instance_id)
    .fetch_one(&mut *tx)
    .await?;

    let key: String = inst.get("definition_key");
    let definition = DefinitionRef {
        id: inst.get("definition_id"),
        key: key.clone(),
    };
    let proc = compiled_process(engine, &mut *tx, definition.id, &key).await?;
    let status = status_from_db(&inst.get::<String, _>("status"))?;

    let mut timers = Vec::new();
    for row in sqlx::query(
        "select timer_no, token_no, element_id, due_kind, due_spec \
         from rbpmn_timer where instance_id = $1 order by timer_no",
    )
    .bind(instance_id)
    .fetch_all(&mut *tx)
    .await?
    {
        let element_id: String = row.get("element_id");
        let spec: String = row.get("due_spec");
        let due = match row.get::<String, _>("due_kind").as_str() {
            "duration" => TimerDue::Duration(spec),
            "date" => TimerDue::Date(spec),
            other => return Err(internal(format!("unknown timer due kind '{other}'"))),
        };
        timers.push((
            TimerId(row.get::<i64, _>("timer_no") as u64),
            TimerState {
                element: proc.node_by_id(&element_id).ok_or_else(|| {
                    internal(format!("timer references unknown element '{element_id}'"))
                })?,
                token: TokenId(row.get::<i64, _>("token_no") as u64),
                due,
            },
        ));
    }

    let mut subscriptions = Vec::new();
    for row in sqlx::query(
        "select subscription_no, token_no, element_id, message_name, correlation_key \
         from rbpmn_subscription where instance_id = $1 order by subscription_no",
    )
    .bind(instance_id)
    .fetch_all(&mut *tx)
    .await?
    {
        let element_id: String = row.get("element_id");
        subscriptions.push((
            SubscriptionId(row.get::<i64, _>("subscription_no") as u64),
            SubscriptionState {
                element: proc.node_by_id(&element_id).ok_or_else(|| {
                    internal(format!(
                        "subscription references unknown element '{element_id}'"
                    ))
                })?,
                token: TokenId(row.get::<i64, _>("token_no") as u64),
                message: row.get("message_name"),
                key: row.get("correlation_key"),
            },
        ));
    }

    let mut tokens = Vec::new();
    for row in sqlx::query(
        "select token_no, element_id, wait_kind, arrived_via, work_item_no \
         from rbpmn_token where instance_id = $1 order by token_no",
    )
    .bind(instance_id)
    .fetch_all(&mut *tx)
    .await?
    {
        let element_id: String = row.get("element_id");
        let node = proc
            .node_by_id(&element_id)
            .ok_or_else(|| internal(format!("token references unknown element '{element_id}'")))?;
        let token_no = TokenId(row.get::<i64, _>("token_no") as u64);
        let wait = match row.get::<String, _>("wait_kind").as_str() {
            "join" => {
                let flow_id: String = row.get("arrived_via");
                WaitKind::Join {
                    arrived_via: proc.flow_by_id(&flow_id).ok_or_else(|| {
                        internal(format!("token references unknown flow '{flow_id}'"))
                    })?,
                }
            }
            "work_item" => WaitKind::WorkItem(WorkItemId(row.get::<i64, _>("work_item_no") as u64)),
            // A token waiting on its own timer/subscription has exactly one,
            // linked back via token_no — resolved here, never guessed.
            "timer" => WaitKind::Timer(
                timers
                    .iter()
                    .find(|(_, t)| t.token == token_no)
                    .map(|(id, _)| *id)
                    .ok_or_else(|| {
                        internal(format!(
                            "token {token_no:?} waits on a timer that has no row"
                        ))
                    })?,
            ),
            "message" => WaitKind::Message(
                subscriptions
                    .iter()
                    .find(|(_, s)| s.token == token_no)
                    .map(|(id, _)| *id)
                    .ok_or_else(|| {
                        internal(format!(
                            "token {token_no:?} waits on a subscription that has no row"
                        ))
                    })?,
            ),
            "event_gateway" => WaitKind::EventGateway,
            "incident" => WaitKind::Incident,
            other => return Err(internal(format!("unknown token wait kind '{other}'"))),
        };
        tokens.push((token_no, Token { node, wait }));
    }

    let mut work_items = Vec::new();
    for row in sqlx::query(
        "select item_no, token_no, element_id, kind, topic from rbpmn_work_item \
         where instance_id = $1 and state in ('available', 'locked') order by item_no",
    )
    .bind(instance_id)
    .fetch_all(&mut *tx)
    .await?
    {
        let element_id: String = row.get("element_id");
        work_items.push((
            WorkItemId(row.get::<i64, _>("item_no") as u64),
            WorkItemState {
                element: proc.node_by_id(&element_id).ok_or_else(|| {
                    internal(format!(
                        "work item references unknown element '{element_id}'"
                    ))
                })?,
                token: TokenId(row.get::<i64, _>("token_no") as u64),
                kind: work_kind_from_db(&row.get::<String, _>("kind"))?,
                topic: row.get("topic"),
                open: true,
            },
        ));
    }

    let state = InstanceState::rehydrate(
        status,
        inst.get("variables"),
        tokens,
        work_items,
        timers,
        subscriptions,
        Counters {
            next_token: inst.get::<i64, _>("next_token") as u64,
            next_work_item: inst.get::<i64, _>("next_work_item") as u64,
            next_timer: inst.get::<i64, _>("next_timer") as u64,
            next_subscription: inst.get::<i64, _>("next_subscription") as u64,
        },
    );
    Ok((definition, proc, state))
}

/// Projects a completed step: instance columns, token snapshot, work-item
/// transitions from the events, and the append-only event rows. Timer and
/// subscription rows follow the events too — armed rows insert (with
/// `due_at` resolved from **database time**), fired/received/cancelled rows
/// delete, in the same transaction as the step that decided it.
pub(crate) async fn persist_step(
    tx: &mut PgConnection,
    proc: &ExecutableProcess,
    definition: &DefinitionRef,
    instance_id: Uuid,
    state: &InstanceState,
    events: &[Event],
) -> Result<(), EngineError> {
    let status = status_to_db(state.status);
    let counters = state.counters();
    sqlx::query(
        "update rbpmn_instance set status = $2, variables = $3, next_token = $4, \
         next_work_item = $5, next_timer = $6, next_subscription = $7, \
         completed_at = case when $2 in ('completed', 'terminated') \
         then now() else completed_at end where id = $1",
    )
    .bind(instance_id)
    .bind(status)
    .bind(&state.variables)
    .bind(counters.next_token as i64)
    .bind(counters.next_work_item as i64)
    .bind(counters.next_timer as i64)
    .bind(counters.next_subscription as i64)
    .execute(&mut *tx)
    .await?;

    // Token rows are a snapshot of the quiescent state (small per instance;
    // wholesale replace keeps the projection trivially correct).
    sqlx::query("delete from rbpmn_token where instance_id = $1")
        .bind(instance_id)
        .execute(&mut *tx)
        .await?;
    for (id, token) in state.tokens() {
        let (wait_kind, arrived_via, work_item_no) = match &token.wait {
            WaitKind::Join { arrived_via } => {
                ("join", Some(proc.flow(*arrived_via).id.clone()), None)
            }
            WaitKind::WorkItem(item) => ("work_item", None, Some(item.0 as i64)),
            WaitKind::Timer(_) => ("timer", None, None),
            WaitKind::Message(_) => ("message", None, None),
            WaitKind::EventGateway => ("event_gateway", None, None),
            WaitKind::Incident => ("incident", None, None),
        };
        sqlx::query(
            "insert into rbpmn_token (instance_id, token_no, element_id, wait_kind, arrived_via, work_item_no) \
             values ($1, $2, $3, $4, $5, $6)",
        )
        .bind(instance_id)
        .bind(id.0 as i64)
        .bind(proc.node_id(token.node))
        .bind(wait_kind)
        .bind(arrived_via)
        .bind(work_item_no)
        .execute(&mut *tx)
        .await?;
    }

    for event in events {
        match event {
            Event::WorkItemCreated {
                id,
                element,
                work_kind,
                topic,
            } => {
                let token_no = state
                    .work_items()
                    .find(|(wid, _)| wid == id)
                    .map(|(_, w)| w.token.0 as i64)
                    .ok_or_else(|| internal("created work item missing from state".into()))?;
                sqlx::query(
                    "insert into rbpmn_work_item \
                     (instance_id, item_no, definition_id, definition_key, token_no, \
                      kind, topic, element_id, state) \
                     values ($1, $2, $3, $4, $5, $6, $7, $8, 'available')",
                )
                .bind(instance_id)
                .bind(id.0 as i64)
                .bind(definition.id)
                .bind(&definition.key)
                .bind(token_no)
                .bind(work_kind.to_string())
                .bind(topic)
                .bind(element)
                .execute(&mut *tx)
                .await?;
                if *work_kind == WorkKind::Service {
                    notify_work(tx, topic).await?;
                }
            }
            Event::WorkItemCompleted { id, .. } => {
                set_work_item_state(tx, instance_id, id.0 as i64, "completed").await?;
            }
            Event::WorkItemCancelled { id, .. } => {
                set_work_item_state(tx, instance_id, id.0 as i64, "cancelled").await?;
            }
            Event::WorkItemFailed { id, .. } => {
                set_work_item_state(tx, instance_id, id.0 as i64, "failed").await?;
            }
            Event::TimerArmed {
                id,
                element,
                due,
                token,
            } => {
                let token_no = token.0 as i64;
                let (due_kind, spec) = match due {
                    TimerDue::Duration(s) => ("duration", s),
                    TimerDue::Date(s) => ("date", s),
                };
                // due_at from database time — the design's clock authority.
                // Both ISO-8601 forms cast natively in PostgreSQL.
                sqlx::query(
                    "insert into rbpmn_timer \
                     (instance_id, timer_no, token_no, element_id, due_kind, due_spec, due_at) \
                     values ($1, $2, $3, $4, $5, $6, case when $5 = 'duration' \
                     then now() + $6::interval else $6::timestamptz end)",
                )
                .bind(instance_id)
                .bind(id.0 as i64)
                .bind(token_no)
                .bind(element)
                .bind(due_kind)
                .bind(spec)
                .execute(&mut *tx)
                .await?;
                // Wake sleeping schedulers: the new timer may be due sooner
                // than the min(due_at) they went to sleep on.
                sqlx::query("select pg_notify('rbpmn_timer', '')")
                    .execute(&mut *tx)
                    .await?;
            }
            Event::TimerFired { id, .. } | Event::TimerCancelled { id, .. } => {
                // Fired: the delete commits with the step — exactly-once.
                sqlx::query("delete from rbpmn_timer where instance_id = $1 and timer_no = $2")
                    .bind(instance_id)
                    .bind(id.0 as i64)
                    .execute(&mut *tx)
                    .await?;
            }
            Event::MessageSubscribed {
                id,
                element,
                message,
                key,
                token,
            } => {
                let token_no = token.0 as i64;
                sqlx::query(
                    "insert into rbpmn_subscription \
                     (instance_id, subscription_no, token_no, element_id, \
                      message_name, correlation_key) \
                     values ($1, $2, $3, $4, $5, $6)",
                )
                .bind(instance_id)
                .bind(id.0 as i64)
                .bind(token_no)
                .bind(element)
                .bind(message)
                .bind(key)
                .execute(&mut *tx)
                .await?;
            }
            Event::MessageReceived { id, .. } | Event::SubscriptionCancelled { id, .. } => {
                sqlx::query(
                    "delete from rbpmn_subscription \
                     where instance_id = $1 and subscription_no = $2",
                )
                .bind(instance_id)
                .bind(id.0 as i64)
                .execute(&mut *tx)
                .await?;
            }
            // Trace/history-only events project no row deltas. Exhaustive
            // on purpose: a new event variant must be classified here
            // deliberately — a wildcard would let a delta-bearing variant
            // compile straight into silent database drift.
            Event::InstanceStarted
            | Event::ElementStarted { .. }
            | Event::ElementCompleted { .. }
            | Event::FlowTaken { .. }
            | Event::VariablesPatched { .. }
            | Event::IncidentRaised { .. }
            | Event::CorrelationFailed { .. }
            | Event::DuplicateSubscription { .. }
            | Event::InstanceCompleted
            | Event::InstanceTerminated => {}
        }
        let payload = serde_json::to_value(event).expect("event serializes");
        let kind = payload
            .get("kind")
            .and_then(|k| k.as_str())
            .unwrap_or("unknown")
            .to_string();
        let element = payload
            .get("element")
            .and_then(|e| e.as_str())
            .map(str::to_string);
        sqlx::query(
            "insert into rbpmn_event (instance_id, definition_id, definition_key, kind, element_id, payload) \
             values ($1, $2, $3, $4, $5, $6)",
        )
        .bind(instance_id)
        .bind(definition.id)
        .bind(&definition.key)
        .bind(kind)
        .bind(element)
        .bind(payload)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// Wakes worker loops (delivered on commit; SKIP LOCKED arbitrates who wins).
async fn notify_work(tx: &mut PgConnection, topic: &str) -> Result<(), EngineError> {
    sqlx::query("select pg_notify('rbpmn_work', $1)")
        .bind(topic)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

async fn set_work_item_state(
    tx: &mut PgConnection,
    instance_id: Uuid,
    item_no: i64,
    to: &str,
) -> Result<(), EngineError> {
    sqlx::query("update rbpmn_work_item set state = $3 where instance_id = $1 and item_no = $2")
        .bind(instance_id)
        .bind(item_no)
        .bind(to)
        .execute(&mut *tx)
        .await?;
    Ok(())
}

async fn insert_engine_event(
    tx: &mut PgConnection,
    definition: &DefinitionRef,
    instance_id: Uuid,
    kind: &str,
    element: &str,
    payload: serde_json::Value,
) -> Result<(), EngineError> {
    sqlx::query(
        "insert into rbpmn_event (instance_id, definition_id, definition_key, kind, element_id, payload) \
         values ($1, $2, $3, $4, $5, $6)",
    )
    .bind(instance_id)
    .bind(definition.id)
    .bind(&definition.key)
    .bind(kind)
    .bind(element)
    .bind(payload)
    .execute(&mut *tx)
    .await?;
    Ok(())
}
