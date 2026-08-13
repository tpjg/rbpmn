//! Thin axum conveniences. The primitive is the renderer; these exist so a
//! host does not have to write the same twelve lines.
//!
//! Three routers, because the three surfaces have genuinely different
//! requirements and bundling them would force the strictest onto all:
//!
//! | router | needs | shows |
//! |---|---|---|
//! | [`editor_router`] | nothing | no data at all — mountable anywhere |
//! | [`environment_router`] | an `Engine` | the covered-topic names |
//! | [`inspector_router`] | an `Engine` | one instance, business data included |
//!
//! **None of them authenticates anybody.** That is the host's job, and the
//! whole reason the inspector inlines its data rather than calling an API:
//! there is no second request to protect, so "can this person reach this
//! path" is the entire access decision. Wrap them in your own middleware and
//! never proxy `/v1` to the same audience — deploy is code.

use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rbpmn_engine::{Engine, EngineError};
use uuid::Uuid;

/// Where [`environment_router`] must be mounted relative to the editor
/// document for the editor's "check against server" button to find it.
pub const EDITOR_API_PREFIX: &str = "api";

/// The editor document. Stateless and constant — no engine, no database, no
/// instance data, so this one is safe to mount anywhere.
///
/// Mount it with `nest`, then add [`editor_slash_redirect`] beside it —
/// axum's `nest` matches the bare prefix but not its trailing-slash form,
/// and a wildcard will not cover the gap because a wildcard needs at least
/// one character to match.
///
/// The catch-all here handles everything *below* the mount: a single-document
/// app has no second page, so showing the tool beats showing an error. A
/// sibling router nested deeper (the API, at [`EDITOR_API_PREFIX`]) still
/// wins — static segments outrank a wildcard.
pub fn editor_router() -> Router {
    Router::new()
        .route("/", get(editor))
        .route("/{*rest}", get(editor))
}

/// The trailing-slash companion to [`editor_router`]. `mount` is the same
/// prefix passed to `nest`; the returned router serves the document at
/// `{mount}/` so both spellings work.
pub fn editor_slash_redirect(mount: &str) -> Router {
    Router::new().route(&format!("{}/", mount.trim_end_matches('/')), get(editor))
}

async fn editor() -> Response {
    html(crate::render_editor())
}

/// The one read the editor can make: which topics the environment covers
/// right now, for the `unresolved-topic` check.
///
/// A list of names, not a validation endpoint — the model stays in the
/// browser. That is not an accident of design: it means a confidential
/// process can be checked against a production environment without being
/// uploaded to it.
pub fn environment_router() -> Router<Engine> {
    Router::new().route("/environment", get(environment))
}

async fn environment(State(engine): State<Engine>) -> Response {
    match engine.covered_topics().await {
        Ok(topics) => Json(serde_json::json!({
            "topics": topics.into_iter().collect::<Vec<String>>(),
        }))
        .into_response(),
        Err(e) => {
            tracing_error("reading the covered topic set", &e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "internal error" })),
            )
                .into_response()
        }
    }
}

/// The read-only inspector, addressed by instance id.
///
/// By UUID and nothing else: finding *which* instance is the application's
/// job — it called `start`, it holds the mapping from its own order or ticket
/// to the id it was given. No list, no search, no pagination.
pub fn inspector_router() -> Router<Engine> {
    Router::new().route("/{id}", get(inspect))
}

async fn inspect(State(engine): State<Engine>, Path(id): Path<Uuid>) -> Response {
    match engine.inspect_instance(id).await {
        Ok(inspection) => html(crate::render_inspection(&inspection)),
        Err(EngineError::UnknownInstance(_)) => (
            StatusCode::NOT_FOUND,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            "no such instance",
        )
            .into_response(),
        Err(e) => {
            tracing_error("rendering an instance inspection", &e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                "internal error",
            )
                .into_response()
        }
    }
}

/// `no-store` because the bytes contain business data, and `nosniff` because
/// they contain a lot of other people's text. `frame-ancestors` is
/// deliberately *not* set here: a meta CSP cannot express it, and only the
/// host knows which of its own origins may frame the document.
fn html(body: String) -> Response {
    let mut response = (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        )],
        body,
    )
        .into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, max-age=0"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

/// The engine's own error detail names internal structure; it belongs in the
/// log, not in a response body.
fn tracing_error(what: &str, e: &dyn std::fmt::Display) {
    tracing::error!(error = %e, "rbpmn-ui: {what} failed");
}
