//! Instance inspection: the read model behind the playground's token-overlay
//! debug view (phase-2 exit criterion) and any dashboard. Read-only.

use crate::runtime::load_instance_snapshot;
use crate::{Engine, EngineError};
use rbpmn_core::{Bindings, OpenIncident};
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
    /// The wiring this instance's definition version was deployed with.
    /// Without it the view can only reveal a topic for work items that were
    /// actually instantiated, so an unreached service task shows nothing —
    /// and since the manifest is deliberately absent from the XML, there is
    /// nowhere else a reader could recover it from.
    pub bindings: Bindings,
    /// The open incident and what a repair would do with it, on a frozen
    /// instance and nowhere else (`docs/design/incident-scope.md`, D10).
    /// Every verdict in it comes from the core functions the repair command
    /// itself asks, so what this says would happen is what happens.
    pub incident: Option<OpenIncident>,
    /// Why a frozen instance carries no `incident`: set exactly when the
    /// status is `failed` and `incident` is `None`. Either the load failed in
    /// Rust — a stored definition that no longer compiles, a row the core
    /// will not rehydrate — or the rows hold no open incident, which no step
    /// produces. `repair` refuses both. Without it the incident vanishes from
    /// the view and its reason reaches only the server's log.
    pub incident_unreadable: Option<String>,
    pub tokens: Vec<TokenView>,
    pub scopes: Vec<ScopeView>,
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
    /// Which runtime scope the token sits in; 0 is the instance root. The
    /// overlay needs it to know *which plane* to draw the token on.
    pub scope_no: i64,
}

/// An open subprocess scope instance — the drill-down planes that are
/// currently live.
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScopeView {
    pub scope_no: i64,
    pub parent_scope_no: i64,
    pub element_id: String,
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
    /// How many failures this item has already taken, and when it becomes
    /// claimable again (RFC 3339, from database time). `retry_at` is the
    /// answer to "why has this not retried yet"; the two policy fields below
    /// are the answer to "and why is the wait that long".
    pub failures: i32,
    pub retry_at: Option<String>,
    /// The retry curve this item was created with, from its definition's
    /// manifest. `None` in both means the engine's own settings — the same
    /// reading the columns have (`docs/design/retry-policy.md`, D6), and what
    /// every item created before per-element policies carries.
    pub backoff_base: Option<f64>,
    pub backoff_multiplier: Option<f64>,
    pub last_failure: Option<String>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EventView {
    pub kind: String,
    pub element_id: Option<String>,
    pub display: String,
    /// Prose an operator needs that is deliberately *not* in `display`.
    /// `Display` is the golden trace format and therefore stable API, so a
    /// reason that will be reworded cannot live there — but it still has to
    /// be reachable, or an incident says only *that* it failed. Today this
    /// carries `timer-resolve-failed`'s reason; anything else has none.
    pub detail: Option<String>,
}

impl Engine {
    pub async fn inspect_instance(&self, id: Uuid) -> Result<InstanceInspection, EngineError> {
        // One repeatable-read transaction: all reads see a single snapshot,
        // so the view can never show e.g. a completed instance with tokens
        // still on it (torn across a concurrent step).
        let mut tx = self.pool().begin().await?;
        sqlx::query("set transaction isolation level repeatable read")
            .execute(&mut *tx)
            .await?;
        let inspection = self.inspect_in(&mut tx, id).await?;
        tx.commit().await?;
        Ok(inspection)
    }

    async fn inspect_in(
        &self,
        tx: &mut sqlx::PgConnection,
        id: Uuid,
    ) -> Result<InstanceInspection, EngineError> {
        let inst = sqlx::query(
            "select i.definition_key, i.status, i.variables, d.bpmn_xml, d.bindings \
             from rbpmn_instance i join rbpmn_definition d on d.id = i.definition_id where i.id = $1",
        )
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(EngineError::UnknownInstance(id))?;

        let definition_key: String = inst.get("definition_key");
        // Unparseable stored wiring is corruption, not drift: `deploy` wrote
        // this from a `Bindings`, and `check_active_definitions` refuses to
        // boot a replica whose stored manifests no longer deserialize. So it
        // is raised rather than defaulted — a debug view that quietly reports
        // "no wiring" would be worse than one that refuses. (The event
        // fallback below is a different case: those payloads span versions.)
        let bindings: Bindings = serde_json::from_value(inst.get("bindings")).map_err(|e| {
            EngineError::CorruptManifest {
                definition_key: definition_key.clone(),
                detail: e.to_string(),
            }
        })?;

        // Only a frozen instance has an incident, and only a frozen one
        // pays for the rehydration this needs.
        let status: String = inst.get("status");
        let (incident, incident_unreadable) = if status == "failed" {
            // Best effort for errors raised in Rust, and only those: this is
            // the view an operator opens *because* something is wrong. A
            // stored definition that no longer compiles, or a row the core
            // will not rehydrate, leaves the rest of the inspection standing,
            // and the instance is unrepairable anyway — `repair` fails on the
            // same load. A database error is returned: a failed statement
            // aborts this transaction, so swallowing it would fail the next
            // read with "current transaction is aborted" and leave the cause
            // in a log line.
            match load_instance_snapshot(self, tx, id).await {
                Ok((_, proc, _, state)) => match rbpmn_core::open_incident(&proc, &state) {
                    Some(incident) => (Some(incident), None),
                    None => (
                        None,
                        Some(
                            "the instance is failed, but its rows hold no open incident \
                             (no incident number, or no token at an incident)"
                                .to_string(),
                        ),
                    ),
                },
                Err(e @ EngineError::Db(_)) => return Err(e),
                Err(e) => {
                    tracing::warn!(instance = %id, error = %e, "cannot read the open incident");
                    (None, Some(e.to_string()))
                }
            }
        } else {
            (None, None)
        };

        let tokens = sqlx::query(
            "select element_id, wait_kind, scope_no from rbpmn_token \
             where instance_id = $1 order by token_no",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(|r| TokenView {
            element_id: r.get("element_id"),
            wait_kind: r.get("wait_kind"),
            scope_no: r.get("scope_no"),
        })
        .collect();

        let scopes = sqlx::query(
            "select scope_no, parent_scope_no, element_id from rbpmn_scope \
             where instance_id = $1 order by scope_no",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(|r| ScopeView {
            scope_no: r.get("scope_no"),
            parent_scope_no: r.get("parent_scope_no"),
            element_id: r.get("element_id"),
        })
        .collect();

        let work_items = sqlx::query(
            "select id, element_id, state, topic, kind, retries, failures, \
             to_char(retry_at at time zone 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') as retry_at, \
             backoff_base, backoff_multiplier, last_failure from rbpmn_work_item \
             where instance_id = $1 order by item_no",
        )
        .bind(id)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(|r| WorkItemView {
            id: r.get("id"),
            element_id: r.get("element_id"),
            state: r.get("state"),
            topic: r.get("topic"),
            kind: r.get("kind"),
            retries: r.get("retries"),
            failures: r.get("failures"),
            retry_at: r.get("retry_at"),
            backoff_base: r.get("backoff_base"),
            backoff_multiplier: r.get("backoff_multiplier"),
            last_failure: r.get("last_failure"),
        })
        .collect();

        let timers = sqlx::query(
            "select element_id, due_spec, \
             to_char(due_at at time zone 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') as due_at \
             from rbpmn_timer where instance_id = $1 order by timer_no",
        )
        .bind(id)
        .fetch_all(&mut *tx)
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
        .fetch_all(&mut *tx)
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
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .map(|r| {
            let kind: String = r.get("kind");
            let element_id: Option<String> = r.get("element_id");
            let payload: serde_json::Value = r.get("payload");
            let detail = payload
                .get("reason")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let display = serde_json::from_value::<rbpmn_core::Event>(payload)
                .map(|e| e.to_string())
                .unwrap_or_else(|_| match &element_id {
                    Some(el) => format!("{kind} {el}"),
                    None => kind.clone(),
                });
            EventView {
                kind,
                element_id,
                display,
                detail,
            }
        })
        .collect();

        Ok(InstanceInspection {
            id,
            definition_key,
            status,
            variables: inst.get("variables"),
            bpmn_xml: inst.get("bpmn_xml"),
            bindings,
            incident,
            incident_unreadable,
            tokens,
            scopes,
            work_items,
            timers,
            subscriptions,
            events,
        })
    }
}
