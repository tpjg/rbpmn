//! Instance inspection: the read model behind the playground's token-overlay
//! debug view (phase-2 exit criterion) and any dashboard. Read-only.

use crate::{Engine, EngineError};
use sqlx::Row;
use uuid::Uuid;

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceInspection {
    pub id: Uuid,
    pub definition_key: String,
    pub status: String,
    pub variables: serde_json::Value,
    pub bpmn_xml: String,
    pub tokens: Vec<TokenView>,
    pub work_items: Vec<WorkItemView>,
    pub timers: Vec<TimerView>,
    pub subscriptions: Vec<SubscriptionView>,
    pub events: Vec<EventView>,
}

/// An armed timer: what is sleeping where, and until when (`due_at` in
/// RFC 3339, from database time).
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TimerView {
    pub element_id: String,
    pub due_spec: String,
    pub due_at: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionView {
    pub element_id: String,
    pub message_name: String,
    pub correlation_key: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenView {
    pub element_id: String,
    pub wait_kind: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkItemView {
    pub id: Uuid,
    pub element_id: String,
    pub state: String,
    pub topic: String,
    pub kind: String,
    pub retries: i32,
    pub last_failure: Option<String>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventView {
    pub kind: String,
    pub element_id: Option<String>,
    pub display: String,
}

impl Engine {
    pub async fn inspect_instance(&self, id: Uuid) -> Result<InstanceInspection, EngineError> {
        let inst = sqlx::query(
            "select i.definition_key, i.status, i.variables, d.bpmn_xml \
             from rbpmn_instance i join rbpmn_definition d on d.id = i.definition_id where i.id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?
        .ok_or(EngineError::UnknownInstance(id))?;

        let tokens = sqlx::query(
            "select element_id, wait_kind from rbpmn_token where instance_id = $1 order by token_no",
        )
        .bind(id)
        .fetch_all(self.pool())
        .await?
        .into_iter()
        .map(|r| TokenView {
            element_id: r.get("element_id"),
            wait_kind: r.get("wait_kind"),
        })
        .collect();

        let work_items = sqlx::query(
            "select id, element_id, state, topic, kind, retries, last_failure from rbpmn_work_item \
             where instance_id = $1 order by item_no",
        )
        .bind(id)
        .fetch_all(self.pool())
        .await?
        .into_iter()
        .map(|r| WorkItemView {
            id: r.get("id"),
            element_id: r.get("element_id"),
            state: r.get("state"),
            topic: r.get("topic"),
            kind: r.get("kind"),
            retries: r.get("retries"),
            last_failure: r.get("last_failure"),
        })
        .collect();

        let timers = sqlx::query(
            "select element_id, due_spec, \
             to_char(due_at at time zone 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') as due_at \
             from rbpmn_timer where instance_id = $1 order by timer_no",
        )
        .bind(id)
        .fetch_all(self.pool())
        .await?
        .into_iter()
        .map(|r| TimerView {
            element_id: r.get("element_id"),
            due_spec: r.get("due_spec"),
            due_at: r.get("due_at"),
        })
        .collect();

        let subscriptions = sqlx::query(
            "select element_id, message_name, correlation_key \
             from rbpmn_subscription where instance_id = $1 order by subscription_no",
        )
        .bind(id)
        .fetch_all(self.pool())
        .await?
        .into_iter()
        .map(|r| SubscriptionView {
            element_id: r.get("element_id"),
            message_name: r.get("message_name"),
            correlation_key: r.get("correlation_key"),
        })
        .collect();

        let events = sqlx::query(
            "select kind, element_id, payload from rbpmn_event where instance_id = $1 order by id",
        )
        .bind(id)
        .fetch_all(self.pool())
        .await?
        .into_iter()
        .map(|r| {
            let kind: String = r.get("kind");
            let element_id: Option<String> = r.get("element_id");
            let display = serde_json::from_value::<rbpmn_core::Event>(r.get("payload"))
                .map(|e| e.to_string())
                .unwrap_or_else(|_| match &element_id {
                    Some(el) => format!("{kind} {el}"),
                    None => kind.clone(),
                });
            EventView {
                kind,
                element_id,
                display,
            }
        })
        .collect();

        Ok(InstanceInspection {
            id,
            definition_key: inst.get("definition_key"),
            status: inst.get("status"),
            variables: inst.get("variables"),
            bpmn_xml: inst.get("bpmn_xml"),
            tokens,
            work_items,
            timers,
            subscriptions,
            events,
        })
    }
}
