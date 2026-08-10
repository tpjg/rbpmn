//! Transactional stepping: instance rows are locked (`FOR UPDATE`), the core
//! state is rebuilt from rows, the pure step runs, and rows + events are
//! written — all in one transaction. Concurrent completions on one instance
//! serialize on the row lock, which is what makes the parallel join's
//! exactly-once firing hold under concurrency (property-tested in the core,
//! integration-tested here).

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

impl Engine {
    pub async fn start(
        &self,
        key: &str,
        business_key: Option<&str>,
        variables: serde_json::Value,
    ) -> Result<StartedInstance, EngineError> {
        let mut tx = self.pool().begin().await?;
        let row = sqlx::query(
            "select id, bpmn_xml, bindings from definition \
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
            "insert into instance \
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
        persist_step(&mut tx, &proc, &definition, instance_id, &state, &events).await?;
        tx.commit().await?;

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

        let item = sqlx::query("select instance_id, item_no from work_item where id = $1")
            .bind(work_item)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(EngineError::UnknownWorkItem(work_item))?;
        let instance_id: Uuid = item.get("instance_id");
        let item_no: i64 = item.get("item_no");

        // Lock the instance first: every step on an instance serializes here.
        let (definition, proc, mut state, raw_status) = load_instance(&mut tx, instance_id).await?;
        if raw_status == "failed" {
            return Err(EngineError::IncidentOpen(instance_id));
        }

        // Re-read the item's state under the lock; closed items answer with
        // the idempotent no-op result before the core is ever involved.
        let item_state: String =
            sqlx::query("select state from work_item where instance_id = $1 and item_no = $2")
                .bind(instance_id)
                .bind(item_no)
                .fetch_one(&mut *tx)
                .await?
                .get("state");
        if item_state != "available" && item_state != "locked" {
            return Ok(Completion::AlreadyClosed { state: item_state });
        }
        if state.status != InstanceStatus::Active {
            return Err(EngineError::InstanceNotActive(
                instance_id,
                format!("{:?}", state.status).to_lowercase(),
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
        persist_step(&mut tx, &proc, &definition, instance_id, &state, &events).await?;
        tx.commit().await?;
        Ok(Completion::Advanced(events))
    }

    /// Handler failure: spend one retry and re-offer, or — budget exhausted —
    /// raise an incident (work item failed, instance in the incident state).
    /// Error-boundary matching joins in the phase-2 follow-up milestone.
    pub async fn fail_work_item(&self, work_item: Uuid) -> Result<FailOutcome, EngineError> {
        let mut tx = self.pool().begin().await?;
        let item = sqlx::query("select instance_id, item_no from work_item where id = $1")
            .bind(work_item)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(EngineError::UnknownWorkItem(work_item))?;
        let instance_id: Uuid = item.get("instance_id");
        let item_no: i64 = item.get("item_no");

        let (definition, _proc, state, raw_status) = load_instance(&mut tx, instance_id).await?;
        if raw_status == "failed" {
            return Err(EngineError::IncidentOpen(instance_id));
        }
        if state.status != InstanceStatus::Active {
            return Err(EngineError::InstanceNotActive(
                instance_id,
                format!("{:?}", state.status).to_lowercase(),
            ));
        }

        let row = sqlx::query(
            "update work_item set retries = retries - 1 \
             where instance_id = $1 and item_no = $2 \
               and state in ('available', 'locked') \
             returning retries, element_id",
        )
        .bind(instance_id)
        .bind(item_no)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(EngineError::UnknownWorkItem(work_item))?;
        let retries: i32 = row.get("retries");
        let element_id: String = row.get("element_id");

        let outcome = if retries > 0 {
            insert_engine_event(
                &mut tx,
                &definition,
                instance_id,
                "work-item-failed",
                &element_id,
                serde_json::json!({ "kind": "work-item-failed", "element": element_id, "retriesLeft": retries }),
            )
            .await?;
            FailOutcome::Retrying {
                retries_left: retries,
            }
        } else {
            sqlx::query(
                "update work_item set state = 'failed' where instance_id = $1 and item_no = $2",
            )
            .bind(instance_id)
            .bind(item_no)
            .execute(&mut *tx)
            .await?;
            sqlx::query("update instance set status = 'failed' where id = $1")
                .bind(instance_id)
                .execute(&mut *tx)
                .await?;
            insert_engine_event(
                &mut tx,
                &definition,
                instance_id,
                "incident-raised",
                &element_id,
                serde_json::json!({ "kind": "incident-raised", "element": element_id }),
            )
            .await?;
            FailOutcome::IncidentRaised
        };
        tx.commit().await?;
        Ok(outcome)
    }
}

fn compile_row(row: &PgRow, key: &str) -> Result<ExecutableProcess, EngineError> {
    let bindings: Bindings =
        serde_json::from_value(row.get::<serde_json::Value, _>("bindings")).unwrap_or_default();
    let defs = rbpmn_model::parse(&row.get::<String, _>("bpmn_xml"))
        .map_err(|e| rbpmn_core::CompileError::Internal(e.to_string()))?;
    Ok(ExecutableProcess::compile(&defs, key, &bindings)?)
}

/// Locks the instance row and rebuilds the quiescent core state from rows —
/// rows are the runtime truth, this is their inverse.
async fn load_instance(
    tx: &mut PgConnection,
    instance_id: Uuid,
) -> Result<(DefinitionRef, ExecutableProcess, InstanceState, String), EngineError> {
    let inst = sqlx::query(
        "select i.definition_id, i.definition_key, i.status, i.variables, \
                i.next_token, i.next_work_item, d.bpmn_xml, d.bindings \
         from instance i join definition d on d.id = i.definition_id \
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

    let raw_status: String = inst.get("status");
    let status = match raw_status.as_str() {
        "active" => InstanceStatus::Active,
        "completed" => InstanceStatus::Completed,
        "terminated" => InstanceStatus::Terminated,
        // 'failed' (incident) is a projection-level refinement of Active;
        // callers gate on the raw status before invoking the core.
        _ => InstanceStatus::Active,
    };

    let tokens = sqlx::query(
        "select token_no, element_id, wait_kind, arrived_via, work_item_no \
         from token where instance_id = $1 order by token_no",
    )
    .bind(instance_id)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|row| {
        let node = proc
            .node_by_id(&row.get::<String, _>("element_id"))
            .expect("token references a model element");
        let wait = match row.get::<String, _>("wait_kind").as_str() {
            "join" => WaitKind::Join {
                arrived_via: proc
                    .flow_by_id(&row.get::<String, _>("arrived_via"))
                    .expect("token references a model flow"),
            },
            _ => WaitKind::WorkItem(WorkItemId(row.get::<i64, _>("work_item_no") as u64)),
        };
        (
            TokenId(row.get::<i64, _>("token_no") as u64),
            Token { node, wait },
        )
    })
    .collect::<Vec<_>>();

    let work_items = sqlx::query(
        "select item_no, token_no, element_id, kind, topic from work_item \
         where instance_id = $1 and state in ('available', 'locked') order by item_no",
    )
    .bind(instance_id)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|row| {
        (
            WorkItemId(row.get::<i64, _>("item_no") as u64),
            WorkItemState {
                element: proc
                    .node_by_id(&row.get::<String, _>("element_id"))
                    .expect("work item references a model element"),
                token: TokenId(row.get::<i64, _>("token_no") as u64),
                kind: match row.get::<String, _>("kind").as_str() {
                    "service" => WorkKind::Service,
                    _ => WorkKind::User,
                },
                topic: row.get("topic"),
                open: true,
            },
        )
    })
    .collect::<Vec<_>>();

    let state = InstanceState::rehydrate(
        status,
        inst.get("variables"),
        tokens,
        work_items,
        inst.get::<i64, _>("next_token") as u64,
        inst.get::<i64, _>("next_work_item") as u64,
    );
    Ok((definition, proc, state, raw_status))
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
    let status = match state.status {
        InstanceStatus::Created => "active",
        InstanceStatus::Active => "active",
        InstanceStatus::Completed => "completed",
        InstanceStatus::Terminated => "terminated",
    };
    sqlx::query(
        "update instance set status = $2, variables = $3, next_token = $4, \
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
    sqlx::query("delete from token where instance_id = $1")
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
            "insert into token (instance_id, token_no, element_id, wait_kind, arrived_via, work_item_no) \
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
                    .expect("created work item is in state");
                sqlx::query(
                    "insert into work_item \
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
            }
            Event::WorkItemCompleted { id, .. } => {
                set_work_item_state(tx, instance_id, id.0 as i64, "completed").await?;
            }
            Event::WorkItemCancelled { id, .. } => {
                set_work_item_state(tx, instance_id, id.0 as i64, "cancelled").await?;
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
            "insert into event (instance_id, definition_id, definition_key, kind, element_id, payload) \
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

async fn set_work_item_state(
    tx: &mut PgConnection,
    instance_id: Uuid,
    item_no: i64,
    to: &str,
) -> Result<(), EngineError> {
    sqlx::query("update work_item set state = $3 where instance_id = $1 and item_no = $2")
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
        "insert into event (instance_id, definition_id, definition_key, kind, element_id, payload) \
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
