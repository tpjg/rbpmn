use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use rbpmn_server::{Tokens, app};
use tower::ServiceExt;

const TOKEN: &str = "test-token-0123456789abcdef-0123456789abcdef";

// The lint corpus is the single source of truth for BPMN test inputs.
const INCLUSIVE_XML: &str =
    include_str!("../../rbpmn-model/tests/fixtures/reject/inclusive-gateway.bpmn");

fn test_app() -> axum::Router {
    app(Tokens::from_list([TOKEN]).unwrap())
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn healthz_is_public_and_static() {
    let resp = test_app()
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["status"], "ok");
    assert!(
        json.get("version").is_none(),
        "healthz must not disclose the version to unauthenticated callers"
    );
}

#[tokio::test]
async fn v1_requires_bearer_token() {
    let resp = test_app()
        .oneshot(
            Request::post("/v1/definitions/lint")
                .body(Body::from(INCLUSIVE_XML))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        resp.headers().get(header::WWW_AUTHENTICATE).unwrap(),
        "Bearer"
    );
}

#[tokio::test]
async fn unknown_v1_paths_cannot_be_probed_without_auth() {
    // Uniform 401: unauthenticated callers cannot distinguish existing
    // routes from non-existent ones.
    let resp = test_app()
        .oneshot(
            Request::post("/v1/definitely/not/a/route")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let resp = test_app()
        .oneshot(
            Request::post("/v1/definitely/not/a/route")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn wrong_token_is_rejected() {
    let resp = test_app()
        .oneshot(
            Request::post("/v1/definitions/lint")
                .header(
                    header::AUTHORIZATION,
                    "Bearer definitely-not-the-token-but-long",
                )
                .body(Body::from(INCLUSIVE_XML))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn lowercase_bearer_scheme_is_accepted() {
    let resp = test_app()
        .oneshot(
            Request::post("/v1/definitions/lint")
                .header(header::AUTHORIZATION, format!("bearer {TOKEN}"))
                .body(Body::from(INCLUSIVE_XML))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn lint_reports_diagnostics() {
    let resp = test_app()
        .oneshot(
            Request::post("/v1/definitions/lint")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::from(INCLUSIVE_XML))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json = body_json(resp).await;
    assert_eq!(json["ok"], false);
    let rules: Vec<&str> = json["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["rule"].as_str().unwrap())
        .collect();
    assert!(rules.contains(&"no-inclusive-gateway"), "got {rules:?}");
}

#[tokio::test]
async fn malformed_xml_is_bad_request() {
    let resp = test_app()
        .oneshot(
            Request::post("/v1/definitions/lint")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::from("this is not xml"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
