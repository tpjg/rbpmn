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
        Err(DeployError::Db(e)) => internal(e),
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
