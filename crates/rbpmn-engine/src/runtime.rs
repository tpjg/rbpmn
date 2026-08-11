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

use crate::{Completion, Engine, EngineError, FailOutcome, StartedInstance};
use rbpmn_core::{
    Bindings, Command, Event, ExecutableProcess, InstanceState, InstanceStatus, Token, TokenId,
    WaitKind, WorkItemId, WorkItemState, WorkKind, step,
};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

struct DefinitionRef {
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
        let row = sqlx::query(
            "select id, bpmn_xml, bindings from rbpmn_definition \
             where key = $1 order by version desc limit 1",
        )
        .bind(key)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| EngineError::UnknownDefinition(key.to_string()))?;

        let definition = DefinitionRef {
            id: row.get("id"),
            key: key.to_string(),
        };
        let proc = compile_row(&row, key)?;

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

    pub async fn complete_work_item(
        &self,
        work_item: Uuid,
        patch: serde_json::Value,
    ) -> Result<Completion, EngineError> {
        let mut tx = self.pool().begin().await?;
        let completion = self
            .complete_work_item_in_tx(&mut tx, work_item, patch)
            .await?;
        tx.commit().await?;
        Ok(completion)
    }

    /// [`Engine::complete_work_item`] inside the caller's transaction.
    pub async fn complete_work_item_in_tx(
        &self,
        tx: &mut PgConnection,
        work_item: Uuid,
        patch: serde_json::Value,
    ) -> Result<Completion, EngineError> {
        reject_nul(&patch)?;
        let item = sqlx::query("select instance_id, item_no from rbpmn_work_item where id = $1")
            .bind(work_item)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(EngineError::UnknownWorkItem(work_item))?;
        let instance_id: Uuid = item.get("instance_id");
        let item_no: i64 = item.get("item_no");

        // Lock the instance first: every step on an instance serializes here.
        let (definition, proc, mut state) = load_instance(&mut *tx, instance_id).await?;

        // The idempotent no-op comes before every other gate: a retried,
        // already-committed completion must converge even if a sibling
        // branch has since raised an incident.
        let item_state: String = sqlx::query(
            "select state from rbpmn_work_item where instance_id = $1 and item_no = $2",
        )
        .bind(instance_id)
        .bind(item_no)
        .fetch_one(&mut *tx)
        .await?
        .get("state");
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
        let item = sqlx::query("select instance_id, item_no from rbpmn_work_item where id = $1")
            .bind(work_item)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(EngineError::UnknownWorkItem(work_item))?;
        let instance_id: Uuid = item.get("instance_id");
        let item_no: i64 = item.get("item_no");

        let (definition, proc, mut state) = load_instance(&mut *tx, instance_id).await?;

        let row = sqlx::query(
            "select state, lock_owner, \
             (lock_until is not null and lock_until > now()) as lease_live \
             from rbpmn_work_item where instance_id = $1 and item_no = $2",
        )
        .bind(instance_id)
        .bind(item_no)
        .fetch_one(&mut *tx)
        .await?;
        let item_state: String = row.get("state");
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
        if item_state == "locked"
            && row.get::<bool, _>("lease_live")
            && options.owner.as_deref() != row.get::<Option<String>, _>("lock_owner").as_deref()
        {
            return Err(EngineError::ItemLeased(work_item));
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

fn compile_row(row: &PgRow, key: &str) -> Result<ExecutableProcess, EngineError> {
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

/// Locks the instance row and rebuilds the quiescent core state from rows —
/// rows are the runtime truth, this is their inverse. Every mapping is
/// exhaustive: an unknown status/kind/wait is an error, never a guess.
async fn load_instance(
    tx: &mut PgConnection,
    instance_id: Uuid,
) -> Result<(DefinitionRef, ExecutableProcess, InstanceState), EngineError> {
    let inst = sqlx::query(
        "select i.definition_id, i.definition_key, i.status, i.variables, \
                i.next_token, i.next_work_item, d.bpmn_xml, d.bindings \
         from rbpmn_instance i join rbpmn_definition d on d.id = i.definition_id \
         where i.id = $1 for update of i",
    )
    .bind(instance_id)
    .fetch_one(&mut *tx)
    .await?;

    let key: String = inst.get("definition_key");
    let definition = DefinitionRef {
        id: inst.get("definition_id"),
        key: key.clone(),
    };
    let proc = compile_row(&inst, &key)?;
    let status = status_from_db(&inst.get::<String, _>("status"))?;

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
            other => return Err(internal(format!("unknown token wait kind '{other}'"))),
        };
        tokens.push((
            TokenId(row.get::<i64, _>("token_no") as u64),
            Token { node, wait },
        ));
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
        inst.get::<i64, _>("next_token") as u64,
        inst.get::<i64, _>("next_work_item") as u64,
    );
    Ok((definition, proc, state))
}

/// Projects a completed step: instance columns, token snapshot, work-item
/// transitions from the events, and the append-only event rows.
async fn persist_step(
    tx: &mut PgConnection,
    proc: &ExecutableProcess,
    definition: &DefinitionRef,
    instance_id: Uuid,
    state: &InstanceState,
    events: &[Event],
) -> Result<(), EngineError> {
    let status = status_to_db(state.status);
    sqlx::query(
        "update rbpmn_instance set status = $2, variables = $3, next_token = $4, \
         next_work_item = $5, completed_at = case when $2 in ('completed', 'terminated') \
         then now() else completed_at end where id = $1",
    )
    .bind(instance_id)
    .bind(status)
    .bind(&state.variables)
    .bind(state.next_token_counter() as i64)
    .bind(state.next_work_item_counter() as i64)
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
            _ => {}
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
