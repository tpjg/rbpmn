//! The engine API surface: everything here mutates business state and sits
//! behind bearer auth. Deploy is atomic (definition + bindings manifest in
//! one body) and idempotent by content; work-item completion answers repeats
//! with the idempotent no-op outcome — clients may retry everything safely.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use rbpmn_engine::{
    Bindings, Completion, DeployError, Engine, EngineError, FailOptions, FailOutcome,
};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeployBody {
    pub bpmn: String,
    #[serde(default)]
    pub bindings: Bindings,
}

pub async fn deploy(State(engine): State<Engine>, Json(body): Json<DeployBody>) -> Response {
    match engine.deploy(&body.bpmn, &body.bindings).await {
        Ok(d) => (
            StatusCode::CREATED,
            Json(json!({
                "definitionId": d.definition_id,
                "key": d.key,
                "version": d.version,
                "reused": d.reused,
                "warnings": d.warnings,
            })),
        )
            .into_response(),
        Err(DeployError::Rejected(diagnostics)) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "diagnostics": diagnostics })),
        )
            .into_response(),
        Err(DeployError::Xml(e)) => bad_request(e.to_string()),
        Err(DeployError::NotExactlyOneProcess(n)) => bad_request(format!(
            "a deployment must contain exactly one process, found {n}"
        )),
        Err(e @ DeployError::InvalidManifest(_)) => bad_request(e.to_string()),
        Err(DeployError::Db(e)) => internal(e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GetTaskBody {
    pub topic: String,
    pub owner: String,
    #[serde(default)]
    pub ttl_seconds: Option<u64>,
    /// "fifo" (default) or "lifo".
    #[serde(default)]
    pub order: Option<String>,
    #[serde(default)]
    pub filter: Option<FilterBody>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FilterBody {
    pub definition_key: String,
    #[serde(default)]
    pub fields: std::collections::BTreeMap<String, String>,
}

impl FilterBody {
    fn into_filter(self) -> rbpmn_engine::TaskFilter {
        let mut filter = rbpmn_engine::TaskFilter::new(self.definition_key);
        for (field, value) in self.fields {
            filter = filter.field(field, value);
        }
        filter
    }
}

/// Claim the next task on a topic (200 with the task, 204 when none).
pub async fn get_task(State(engine): State<Engine>, Json(body): Json<GetTaskBody>) -> Response {
    let order = match body.order.as_deref() {
        None | Some("fifo") => rbpmn_engine::TaskOrder::Fifo,
        Some("lifo") => rbpmn_engine::TaskOrder::Lifo,
        Some(other) => return bad_request(format!("unknown order '{other}' (fifo|lifo)")),
    };
    let mut options = rbpmn_engine::GetTaskOptions::new(body.owner);
    if let Some(secs) = body.ttl_seconds {
        options.ttl = std::time::Duration::from_secs(secs);
    }
    options.order = order;
    options.filter = body.filter.map(FilterBody::into_filter);
    match engine.get_task(&body.topic, &options).await {
        Ok(Some(task)) => Json(serde_json::json!({ "task": task })).into_response(),
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => engine_error(e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CountTasksBody {
    pub topic: String,
    #[serde(default)]
    pub filter: Option<FilterBody>,
}

pub async fn count_tasks(
    State(engine): State<Engine>,
    Json(body): Json<CountTasksBody>,
) -> Response {
    let filter = body.filter.map(FilterBody::into_filter);
    match engine.count_tasks(&body.topic, filter.as_ref()).await {
        Ok(count) => Json(serde_json::json!({ "count": count })).into_response(),
        Err(e) => engine_error(e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtendBody {
    pub owner: String,
    pub ttl_seconds: u64,
}

/// Heartbeat. A lost lease is 409 with a distinct outcome — the client's UI
/// must be able to say "this task was reassigned", never fail silently.
pub async fn extend_lock(
    State(engine): State<Engine>,
    Path(id): Path<Uuid>,
    Json(body): Json<ExtendBody>,
) -> Response {
    match engine
        .extend_lock(
            id,
            &body.owner,
            std::time::Duration::from_secs(body.ttl_seconds),
        )
        .await
    {
        Ok(rbpmn_engine::LockExtension::Extended { until }) => {
            Json(json!({ "outcome": "extended", "lockUntil": until })).into_response()
        }
        Ok(rbpmn_engine::LockExtension::Lost) => {
            (StatusCode::CONFLICT, Json(json!({ "outcome": "lockLost" }))).into_response()
        }
        Err(e) => engine_error(e),
    }
}

#[derive(Deserialize)]
pub struct CompleteTaskBody {
    pub owner: String,
    #[serde(default)]
    pub patch: Option<serde_json::Value>,
}

/// Owner-checked completion (the ownerless push-mode endpoint stays at
/// /work-items/{id}/complete).
pub async fn complete_task(
    State(engine): State<Engine>,
    Path(id): Path<Uuid>,
    Json(body): Json<CompleteTaskBody>,
) -> Response {
    let patch = body.patch.unwrap_or_else(|| json!({}));
    match engine.complete_task(id, &body.owner, patch).await {
        Ok(Completion::Advanced(_)) => Json(json!({ "outcome": "advanced" })).into_response(),
        Ok(Completion::AlreadyClosed { state }) => {
            Json(json!({ "outcome": "alreadyClosed", "state": state })).into_response()
        }
        Err(e) => engine_error(e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailTaskBody {
    pub owner: String,
    #[serde(default)]
    pub error_code: Option<String>,
    #[serde(default)]
    pub error_message: Option<String>,
}

pub async fn fail_task(
    State(engine): State<Engine>,
    Path(id): Path<Uuid>,
    Json(body): Json<FailTaskBody>,
) -> Response {
    match engine
        .fail_task(id, &body.owner, body.error_code, body.error_message)
        .await
    {
        Ok(FailOutcome::Retrying { retries_left }) => {
            Json(json!({ "outcome": "retrying", "retriesLeft": retries_left })).into_response()
        }
        Ok(FailOutcome::AlreadyClosed { state }) => {
            Json(json!({ "outcome": "alreadyClosed", "state": state })).into_response()
        }
        Ok(FailOutcome::ErrorCaught(_)) => {
            Json(json!({ "outcome": "errorCaught" })).into_response()
        }
        Ok(FailOutcome::IncidentRaised) => {
            Json(json!({ "outcome": "incidentRaised" })).into_response()
        }
        Err(e) => engine_error(e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartBody {
    pub definition_key: String,
    #[serde(default)]
    pub business_key: Option<String>,
    #[serde(default)]
    pub variables: Option<serde_json::Value>,
}

pub async fn start(State(engine): State<Engine>, Json(body): Json<StartBody>) -> Response {
    let variables = body.variables.unwrap_or_else(|| json!({}));
    match engine
        .start(
            &body.definition_key,
            body.business_key.as_deref(),
            variables,
        )
        .await
    {
        Ok(started) => (
            StatusCode::CREATED,
            Json(json!({ "instanceId": started.id })),
        )
            .into_response(),
        Err(e) => engine_error(e),
    }
}

pub async fn inspect(State(engine): State<Engine>, Path(id): Path<Uuid>) -> Response {
    match engine.inspect_instance(id).await {
        Ok(inspection) => Json(inspection).into_response(),
        Err(e) => engine_error(e),
    }
}

#[derive(Deserialize)]
pub struct CompleteBody {
    #[serde(default)]
    pub patch: Option<serde_json::Value>,
}

pub async fn complete(
    State(engine): State<Engine>,
    Path(id): Path<Uuid>,
    Json(body): Json<CompleteBody>,
) -> Response {
    let patch = body.patch.unwrap_or_else(|| json!({}));
    match engine.complete_work_item(id, patch).await {
        Ok(Completion::Advanced(_)) => Json(json!({ "outcome": "advanced" })).into_response(),
        Ok(Completion::AlreadyClosed { state }) => {
            Json(json!({ "outcome": "alreadyClosed", "state": state })).into_response()
        }
        Err(e) => engine_error(e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FailBody {
    #[serde(default)]
    pub error_code: Option<String>,
    /// Recorded on the work item and in retry events — makes incidents
    /// diagnosable from stored state.
    #[serde(default)]
    pub error_message: Option<String>,
}

pub async fn fail(
    State(engine): State<Engine>,
    Path(id): Path<Uuid>,
    Json(body): Json<FailBody>,
) -> Response {
    // HTTP callers carry no lease identity: failing a live-leased item is
    // refused (409) so a stray call cannot yank work from a running worker.
    let options = FailOptions {
        error_code: body.error_code,
        detail: body.error_message,
        owner: None,
    };
    match engine.fail_work_item(id, &options).await {
        Ok(FailOutcome::Retrying { retries_left }) => {
            Json(json!({ "outcome": "retrying", "retriesLeft": retries_left })).into_response()
        }
        Ok(FailOutcome::AlreadyClosed { state }) => {
            Json(json!({ "outcome": "alreadyClosed", "state": state })).into_response()
        }
        Ok(FailOutcome::ErrorCaught(_)) => {
            Json(json!({ "outcome": "errorCaught" })).into_response()
        }
        Ok(FailOutcome::IncidentRaised) => {
            Json(json!({ "outcome": "incidentRaised" })).into_response()
        }
        Err(e) => engine_error(e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageBody {
    pub name: String,
    pub correlation_key: String,
    #[serde(default)]
    pub patch: Option<serde_json::Value>,
}

/// Message ingress: deliver to the single open subscription matching
/// (name, correlation key). No match is 404 — the message had nowhere to go,
/// said loudly, never dropped; more than one match is 409 (delivering to
/// "one of them" would be a guess).
pub async fn message(State(engine): State<Engine>, Json(body): Json<MessageBody>) -> Response {
    let patch = body.patch.unwrap_or_else(|| json!({}));
    match engine
        .correlate(&body.name, &body.correlation_key, patch)
        .await
    {
        Ok(correlation) => Json(json!({ "instanceId": correlation.instance_id })).into_response(),
        Err(e) => engine_error(e),
    }
}

#[derive(Deserialize)]
pub struct TopicBody {
    pub name: String,
}

/// Grows the environment at runtime (idempotent). The declaration is
/// persisted, so it survives restarts and is visible to every replica —
/// config and API declarations converge on the same set.
pub async fn declare_topic(State(engine): State<Engine>, Json(body): Json<TopicBody>) -> Response {
    match engine.declare_topic(body.name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(e),
    }
}

fn engine_error(e: EngineError) -> Response {
    let (status, message) = match &e {
        EngineError::UnknownDefinition(_)
        | EngineError::UnknownWorkItem(_)
        | EngineError::UnknownInstance(_) => (StatusCode::NOT_FOUND, e.to_string()),
        EngineError::IncidentOpen(_) | EngineError::InstanceNotActive(..) => {
            (StatusCode::CONFLICT, e.to_string())
        }
        EngineError::ItemLeased(_) => (StatusCode::CONFLICT, e.to_string()),
        EngineError::NoSubscription { .. } => (StatusCode::NOT_FOUND, e.to_string()),
        EngineError::AmbiguousCorrelation { .. } => (StatusCode::CONFLICT, e.to_string()),
        EngineError::InvalidVariables(_) => (StatusCode::BAD_REQUEST, e.to_string()),
        EngineError::Compile(_) | EngineError::Step(_) => {
            (StatusCode::UNPROCESSABLE_ENTITY, e.to_string())
        }
        EngineError::Db(inner) => {
            // The generic 500 hides internals from callers, not operators.
            tracing::error!(error = %inner, "database error serving engine API");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_string(),
            )
        }
        // Startup-only in practice (migrate runs before serve); still mapped
        // so nothing leaks through a wildcard.
        EngineError::MigrationDrift(..) => {
            tracing::error!(error = %e, "migration drift surfaced via API");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_string(),
            )
        }
    };
    (status, Json(json!({ "error": message }))).into_response()
}

fn bad_request(message: String) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": message }))).into_response()
}

fn internal(e: impl std::fmt::Display) -> Response {
    // Never leak internals; the detail goes to the log.
    tracing::error!(error = %e, "internal error");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": "internal error" })),
    )
        .into_response()
}
