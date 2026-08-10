//! rbpmn-server: a small, security-conscious HTTP wrapper around the rbpmn
//! engine, for deployments that don't embed the Rust library directly.
//!
//! Security posture (see docs/http-security.md at the repo root):
//!   * everything under /v1 requires a static bearer token, compared in
//!     constant time against SHA-256 hashes (multiple tokens allowed, for
//!     rotation); unknown /v1 paths also answer 401 before 404
//!   * plain HTTP, loopback bind by default; non-loopback binds are refused
//!     unless explicitly allowed, and are expected to sit behind a
//!     TLS-terminating reverse proxy
//!   * request body limit, request timeout, Authorization never logged
//!
//! Endpoints grow with the engine phases; today only the linter is exposed.

mod api;

use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router, extract::Request, extract::State};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::iter::once;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use subtle::ConstantTimeEq;
use tower::ServiceBuilder;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::sensitive_headers::SetSensitiveRequestHeadersLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

/// Deploy payloads are BPMN XML; 5 MiB is generous for any real model.
const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Below this length a token is guessable enough that we refuse to start.
const MIN_TOKEN_CHARS: usize = 32;

pub const DEFAULT_BIND: &str = "127.0.0.1:7420";

/// Accepted API tokens, stored only as SHA-256 hashes.
pub struct Tokens {
    hashes: Vec<[u8; 32]>,
}

impl Tokens {
    /// Comma-separated form (`RBPMN_API_TOKEN`): every entry must be a token —
    /// an empty entry is a configuration mistake, not a comment.
    pub fn from_list<I>(entries: I) -> Result<Self, String>
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        let mut hashes = Vec::new();
        for entry in entries {
            let token = entry.as_ref().trim();
            if token.is_empty() {
                return Err("empty token entry in RBPMN_API_TOKEN (stray comma?)".to_string());
            }
            hashes.push(Self::hash_checked(token)?);
        }
        Self::non_empty(hashes)
    }

    /// One-token-per-line form (`RBPMN_API_TOKEN_FILE`): blank lines and `#`
    /// comments are allowed here, and only here.
    pub fn from_lines<I>(lines: I) -> Result<Self, String>
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
    {
        let mut hashes = Vec::new();
        for line in lines {
            let token = line.as_ref().trim();
            if token.is_empty() || token.starts_with('#') {
                continue;
            }
            hashes.push(Self::hash_checked(token)?);
        }
        Self::non_empty(hashes)
    }

    pub fn from_env() -> Result<Self, String> {
        if let Ok(value) = std::env::var("RBPMN_API_TOKEN") {
            return Self::from_list(value.split(','));
        }
        if let Ok(path) = std::env::var("RBPMN_API_TOKEN_FILE") {
            let content = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read RBPMN_API_TOKEN_FILE '{path}': {e}"))?;
            return Self::from_lines(content.lines());
        }
        Err(
            "no API token configured: set RBPMN_API_TOKEN (comma-separated for rotation) \
             or RBPMN_API_TOKEN_FILE (one token per line)"
                .to_string(),
        )
    }

    fn hash_checked(token: &str) -> Result<[u8; 32], String> {
        let chars = token.chars().count();
        if chars < MIN_TOKEN_CHARS {
            return Err(format!(
                "API token too short ({chars} chars, minimum {MIN_TOKEN_CHARS}) — \
                 generate one with `openssl rand -hex 32`"
            ));
        }
        Ok(Sha256::digest(token.as_bytes()).into())
    }

    fn non_empty(hashes: Vec<[u8; 32]>) -> Result<Self, String> {
        if hashes.is_empty() {
            return Err("no API tokens configured".to_string());
        }
        Ok(Tokens { hashes })
    }

    /// Constant-time membership test: hash the presented token, compare
    /// against every stored hash without short-circuiting.
    pub fn verify(&self, presented: &str) -> bool {
        let h: [u8; 32] = Sha256::digest(presented.as_bytes()).into();
        let mut ok = subtle::Choice::from(0u8);
        for stored in &self.hashes {
            ok |= h.as_slice().ct_eq(stored.as_slice());
        }
        ok.into()
    }

    pub fn token_count(&self) -> usize {
        self.hashes.len()
    }
}

/// Full server configuration; env-only — secrets never come from CLI args.
pub struct Config {
    pub bind: SocketAddr,
    pub tokens: Tokens,
    pub allow_non_loopback: bool,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let bind_raw = std::env::var("RBPMN_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());
        Ok(Config {
            bind: resolve_bind(&bind_raw)?,
            tokens: Tokens::from_env()?,
            allow_non_loopback: std::env::var("RBPMN_ALLOW_NON_LOOPBACK")
                .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true")),
        })
    }
}

/// Resolves `host:port` — hostnames included, so `localhost:7420` works.
pub fn resolve_bind(raw: &str) -> Result<SocketAddr, String> {
    use std::net::ToSocketAddrs;
    raw.to_socket_addrs()
        .map_err(|e| {
            format!("invalid RBPMN_BIND '{raw}': {e} (expected host:port, e.g. {DEFAULT_BIND})")
        })?
        .next()
        .ok_or_else(|| format!("RBPMN_BIND '{raw}' resolved to no addresses"))
}

/// The server speaks plain HTTP: anything beyond loopback must be a
/// deliberate decision, with TLS termination in front.
pub fn validate_bind(addr: &SocketAddr, allow_non_loopback: bool) -> Result<(), String> {
    if addr.ip().is_loopback() || allow_non_loopback {
        Ok(())
    } else {
        Err(format!(
            "refusing to bind {addr}: not a loopback address. rbpmn-server speaks \
             plain HTTP; expose it beyond localhost only behind a TLS-terminating \
             reverse proxy, then set RBPMN_ALLOW_NON_LOOPBACK=true. \
             See docs/http-security.md"
        ))
    }
}

/// Validates the bind policy, binds, and serves until Ctrl-C/SIGTERM.
/// The only way this crate starts a listener — the loopback check cannot be
/// forgotten by a caller because it is not a separate step.
pub async fn serve(config: Config, engine: rbpmn_engine::Engine) -> Result<(), String> {
    validate_bind(&config.bind, config.allow_non_loopback)?;
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(|e| format!("cannot bind {}: {e}", config.bind))?;
    tracing::info!(bind = %config.bind, tokens = config.tokens.token_count(), "rbpmn-server listening");
    axum::serve(listener, app(config.tokens, engine))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| format!("server error: {e}"))
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl-C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received");
}

struct AppState {
    tokens: Tokens,
}

type SharedState = Arc<AppState>;

pub fn app(tokens: Tokens, engine: rbpmn_engine::Engine) -> Router {
    let state: SharedState = Arc::new(AppState { tokens });

    // Auth is structural, not conventional: the layer wraps the whole nested
    // /v1 router including its fallback, so any future /v1 route — and any
    // unknown /v1 path — hits require_bearer first. Unauthenticated callers
    // get a uniform 401 and cannot probe which routes exist.
    let v1 = Router::new()
        .route("/definitions/lint", post(lint_definitions))
        .route("/definitions", post(api::deploy))
        .route("/instances", post(api::start))
        .route("/instances/{id}/inspect", get(api::inspect))
        .route("/work-items/{id}/complete", post(api::complete))
        .route("/work-items/{id}/fail", post(api::fail))
        .route("/topics", post(api::declare_topic))
        .with_state(engine)
        .fallback(api_not_found)
        .layer(middleware::from_fn_with_state(state, require_bearer));

    Router::new()
        .route("/healthz", get(healthz))
        .nest("/v1", v1)
        .layer(
            // ServiceBuilder: first layer is outermost. Sensitive-headers must
            // wrap TraceLayer so the Authorization value never reaches logs.
            ServiceBuilder::new()
                .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
                .layer(SetSensitiveRequestHeadersLayer::new(once(
                    header::AUTHORIZATION,
                )))
                .layer(TraceLayer::new_for_http())
                .layer(TimeoutLayer::with_status_code(
                    StatusCode::REQUEST_TIMEOUT,
                    REQUEST_TIMEOUT,
                ))
                .layer(PropagateRequestIdLayer::x_request_id()),
        )
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
}

/// RFC 7235: the auth scheme is case-insensitive.
fn bearer_token(header_value: &str) -> Option<&str> {
    let (scheme, rest) = header_value.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") {
        Some(rest.trim())
    } else {
        None
    }
}

async fn require_bearer(State(state): State<SharedState>, req: Request, next: Next) -> Response {
    let authorized = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(bearer_token)
        .is_some_and(|token| state.tokens.verify(token));

    if authorized {
        next.run(req).await
    } else {
        let mut resp = (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "unauthorized" })),
        )
            .into_response();
        resp.headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        resp
    }
}

/// Unauthenticated liveness probe: static status only — no version, no
/// internals (see docs/http-security.md).
async fn healthz() -> Response {
    Json(json!({ "status": "ok", "service": "rbpmn-server" })).into_response()
}

async fn api_not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": "not found" }))).into_response()
}

/// Lint a BPMN document without deploying it: exactly the diagnostics deploy
/// would produce. 200 even when the model is rejected — the linting itself
/// succeeded and the diagnostics are the payload; 400 only for input that is
/// not a BPMN XML document at all. Linting is CPU-bound, so it runs on the
/// blocking pool rather than pinning an async worker.
async fn lint_definitions(body: String) -> Response {
    match tokio::task::spawn_blocking(move || rbpmn_model::check(&body)).await {
        Ok(Ok(checked)) => Json(json!({
            "ok": checked.ok,
            "diagnostics": checked.diagnostics,
        }))
        .into_response(),
        Ok(Err(e)) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "internal error" })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_policy() {
        assert!(Tokens::from_list(["too-short"]).is_err());
        assert!(Tokens::from_list(Vec::<String>::new()).is_err());
        // Env form: empty entries are mistakes, not comments.
        assert!(Tokens::from_list(["0123456789abcdef0123456789abcdef", ""]).is_err());
        assert!(Tokens::from_list(["#0123456789abcdef0123456789abcdef"]).is_ok());
        // File form: blanks and comments are skipped.
        assert!(Tokens::from_lines(["", "# comment"]).is_err());
        assert!(
            Tokens::from_lines(["# rotation set", "", "0123456789abcdef0123456789abcdef"]).is_ok()
        );

        let t = Tokens::from_list([
            "0123456789abcdef0123456789abcdef",
            "another-token-that-is-long-enough-000",
        ])
        .unwrap();
        assert_eq!(t.token_count(), 2);
        assert!(t.verify("0123456789abcdef0123456789abcdef"));
        assert!(t.verify("another-token-that-is-long-enough-000"));
        assert!(!t.verify("0123456789abcdef0123456789abcdeX"));
        assert!(!t.verify(""));
    }

    #[test]
    fn bearer_scheme_is_case_insensitive() {
        assert_eq!(bearer_token("Bearer abc"), Some("abc"));
        assert_eq!(bearer_token("bearer abc"), Some("abc"));
        assert_eq!(bearer_token("BEARER  abc "), Some("abc"));
        assert_eq!(bearer_token("Basic abc"), None);
        assert_eq!(bearer_token("Bearerabc"), None);
    }

    #[test]
    fn bind_policy() {
        let loopback: SocketAddr = "127.0.0.1:7420".parse().unwrap();
        let all: SocketAddr = "0.0.0.0:7420".parse().unwrap();
        let v6_loopback: SocketAddr = "[::1]:7420".parse().unwrap();
        assert!(validate_bind(&loopback, false).is_ok());
        assert!(validate_bind(&v6_loopback, false).is_ok());
        assert!(validate_bind(&all, false).is_err());
        assert!(validate_bind(&all, true).is_ok());
    }

    #[test]
    fn bind_resolution_accepts_hostnames() {
        assert!(resolve_bind("127.0.0.1:7420").is_ok());
        let localhost = resolve_bind("localhost:7420").unwrap();
        assert!(localhost.ip().is_loopback());
        assert!(resolve_bind("7420").is_err());
        assert!(resolve_bind("definitely-not-a-real-host.invalid:7420").is_err());
    }
}
